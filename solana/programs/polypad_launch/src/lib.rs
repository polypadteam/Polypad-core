//! Polypad launch on Solana: `LaunchFactory`, `FeeVault` and the Graduator's
//! fee role (docs/SOLANA.md, docs/solana/EVM-PORT-SPEC.md §4, §7-9), with
//! the curve and the graduated pool on Meteora DBC and DAMM v2
//! (docs/solana/METEORA.md) instead of our own.
//!
//! A launch is what the API builds (METEORA.md "Instruction flow"):
//!
//! ```text
//! tx A  ed25519(quote)                 the pricer's BUY quote
//!       polypad_launch::launch         quote checked, market ensured, DBC config
//!                                      made for this coin, record written,
//!                                      optional first-buy pToken mint (exchange CPI)
//!       DBC initialize_virtual_pool_…  the coin and its pool (when there is no first buy)
//! tx B  DBC initialize_virtual_pool_…  (with a first buy: the pool here, in a
//!       ATA create, DBC swap2           Jito bundle after tx A) and the creator's
//!                                      buy, at the curve fee
//! ```
//!
//! DBC charges a buy the curve fee instead of the anti-sniper schedule's
//! start only when the pool creation and that buy are top-level DBC
//! instructions of one transaction, and that transaction plus the launch
//! does not fit in 1,232 bytes, hence the bundle. The coin's mint signs
//! `launch`, so only the creator can create the pool `launch` recorded;
//! until they do there is nothing to snipe. The pool must be opened before
//! the launch quote expires: one opened later sits on a curve sized at a
//! stale price, so it never collects or graduates here, and anyone can
//! close its record (`expire_launch`).
//!
//! Differences from the EVM a client must know:
//!
//! - Graduation is DBC's: once the curve completes every swap is refused
//!   until DBC's `migration_damm_v2` runs. Our keeper's `graduate` runs it
//!   by CPI through the pinned `Hv8L…` config (Meteora's keepers skip pToken
//!   quotes); anyone else may run it first, and the keeper's `graduate`
//!   then only records the pool and DBC's first position. Either way it
//!   checks the fee vault's position holds, locked for good, exactly the
//!   liquidity the migration made, so nothing others add to the pool can
//!   block it.
//! - Partner fees on pools that are not coins (more pools on a coin's
//!   config, look-alike configs naming our vault, expired launches) and the
//!   fees of positions such pools' migrations hand our vault are swept to
//!   the platform by the keeper (`sweep_fees`, `sweep_position_fees`),
//!   never in a coin's own mint.
//! - Fees arrive in pToken only and are claimed by `collect` (anyone), split
//!   creator side / platform per the coin's `Fees`; the holders' part of the
//!   creator side waits for the keeper's off-chain payout (decision D3).
//! - Payees take their pToken with `withdraw`; the API pairs it with the
//!   exchange's `redeem` for USDC (the EVM `withdrawUsd`).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT};
use anchor_lang::solana_program::program_pack::Pack;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::{self, spl_token, Burn, TransferChecked};
use polypad_exchange as px;
use px::quote::{Quote, BUY};

pub mod contexts;
pub mod curve;
pub mod error;
pub mod events;
pub mod meteora;
pub mod state;

pub use contexts::*;
use error::LaunchError;
use events::*;
use state::*;

declare_id!("GDe9PRUW6Tq2McNSLTfXMETk4PgjHhyCYm3JvneKxLzn");

declare_program!(dynamic_bonding_curve);
declare_program!(cp_amm);

use dynamic_bonding_curve as dbc;

fn now() -> Result<i64> {
    Ok(Clock::get()?.unix_timestamp)
}

/// Send `amount` of `mint` from a fee-vault token account.
#[allow(clippy::too_many_arguments)]
fn vault_transfer<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    decimals: u8,
    to: &AccountInfo<'info>,
    fee_vault: &AccountInfo<'info>,
    vault_bump: u8,
    amount: u64,
) -> Result<()> {
    let bump = [vault_bump];
    let seeds: &[&[u8]] = &[FEE_VAULT_SEED, &bump];
    token::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked { from: from.clone(), mint: mint.clone(), to: to.clone(), authority: fee_vault.clone() },
            &[seeds],
        ),
        amount,
        decimals,
    )
}

/// Whether the coin's DBC pool was opened by its launch quote's expiry:
/// in the launch transaction or its bundle, never later.
fn opened_in_time(s: &meteora::CurveState, l: &Launch) -> bool {
    i64::try_from(s.activation_point).is_ok_and(|t| t <= l.pool_deadline)
}

fn credit(owed: &mut Owed, mint: Pubkey, payee: Pubkey, platform: bool, bump: u8, amount: u64) -> Result<()> {
    if owed.payee == Pubkey::default() {
        owed.mint = mint;
        owed.payee = payee;
        owed.bump = bump;
        owed.platform = platform;
    }
    owed.amount = owed.amount.checked_add(amount).ok_or(LaunchError::MathOverflow)?;
    Ok(())
}

#[program]
pub mod polypad_launch {
    use super::*;

    /* ------------------------------------------------------------ setup */

    /// Create the factory. Only the program's upgrade authority can, so
    /// nobody can front-run the deployment with their own owner.
    pub fn initialize(ctx: Context<Initialize>, owner: Pubkey, keeper: Pubkey, platform: Pubkey, holder_payout: Pubkey) -> Result<()> {
        require!(owner != Pubkey::default() && platform != Pubkey::default(), LaunchError::BadParams);
        let f = &mut ctx.accounts.factory;
        f.owner = owner;
        f.keeper = keeper;
        f.platform = platform;
        f.holder_payout = holder_payout;
        f.grad_usd = DEFAULT_GRAD_USD;
        f.fees = Fees::DEFAULT;
        f.bump = ctx.bumps.factory;
        f.vault_bump = ctx.bumps.fee_vault;
        emit_cpi!(ConfigSet { platform, grad_usd: DEFAULT_GRAD_USD });
        emit_cpi!(FeesSet { fees: Fees::DEFAULT });
        emit_cpi!(RolesSet { keeper, holder_payout });
        Ok(())
    }

    /* ----------------------------------------------------------- owner */

    /// Two-step ownership; there is no renounce (as on the EVM factory).
    pub fn transfer_ownership(ctx: Context<OwnerOnly>, new_owner: Pubkey) -> Result<()> {
        let f = &mut ctx.accounts.factory;
        f.pending_owner = new_owner;
        let owner = f.owner;
        emit_cpi!(OwnershipTransferStarted { owner, pending_owner: new_owner });
        Ok(())
    }

    pub fn accept_ownership(ctx: Context<AcceptOwnership>) -> Result<()> {
        let f = &mut ctx.accounts.factory;
        let previous = f.owner;
        f.owner = f.pending_owner;
        f.pending_owner = Pubkey::default();
        let owner = f.owner;
        emit_cpi!(OwnershipTransferred { previous, owner });
        Ok(())
    }

    /// `setConfig`: the platform payee and the graduation size, $1k-$1M.
    /// Coins already launched keep their curve; owed fees stay with the old
    /// platform. The platform may be any wallet, a creator's or payee's
    /// included: its `Owed` accounts have their own seed, so `collect`
    /// credits both sides apart.
    pub fn set_config(ctx: Context<OwnerOnly>, platform: Pubkey, grad_usd: u64) -> Result<()> {
        require!(platform != Pubkey::default() && (MIN_GRAD_USD..=MAX_GRAD_USD).contains(&grad_usd), LaunchError::BadParams);
        let f = &mut ctx.accounts.factory;
        f.platform = platform;
        f.grad_usd = grad_usd;
        emit_cpi!(ConfigSet { platform, grad_usd });
        Ok(())
    }

    /// `setFees`, for coins launched from now on: the curve fee 0.25-2%,
    /// each creator share 30-80%; the pool fee is the DAMM v2 config's 1%.
    pub fn set_fees(ctx: Context<OwnerOnly>, fees: Fees) -> Result<()> {
        require!(fees.valid(), LaunchError::BadFees);
        ctx.accounts.factory.fees = fees;
        emit_cpi!(FeesSet { fees });
        Ok(())
    }

    pub fn set_roles(ctx: Context<OwnerOnly>, keeper: Pubkey, holder_payout: Pubkey) -> Result<()> {
        let f = &mut ctx.accounts.factory;
        f.keeper = keeper;
        f.holder_payout = holder_payout;
        emit_cpi!(RolesSet { keeper, holder_payout });
        Ok(())
    }

    /* ---------------------------------------------------------- launch */

    /// `LaunchFactory.launch`: a coin on the market of `quote`, its curve
    /// sized at the quoted BUY price. The transaction's previous instruction
    /// must be the ed25519 verification of `quote` by the exchange's signer
    /// (the quote is not consumed, as on the EVM). `base_mint` is the coin's
    /// fresh mint keypair, which DBC's pool creation then initializes. With
    /// `first_buy > 0` the creator also mints pTokens for `first_buy` USDC at
    /// the same quote, for the DBC `swap2` that follows the pool creation.
    pub fn launch<'info>(
        ctx: Context<'info, LaunchCoin<'info>>,
        quote: Quote,
        holders_bps: u16,
        first_buy: u64,
        first_buy_min_out: u64,
    ) -> Result<()> {
        // The quote is the precompile instruction before the top-level one;
        // only a top-level launch is that instruction's direct successor.
        require!(get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT, LaunchError::NotTopLevel);
        require!(holders_bps as u64 <= BPS, LaunchError::BadHoldersShare);
        let t = now()?;
        let a = &mut *ctx.accounts;
        let creator = a.creator.key();

        // `_launchPrice`: the exchange's quote check, then its band.
        let cfg = &a.exchange_config;
        require!(cfg.signer != Pubkey::default(), LaunchError::NoSigner);
        let ixs = a.instructions.to_account_info();
        let price = px::quote::verify(&ixs, &quote, BUY, &cfg.signer, &px::ID, t).map_err(LaunchError::from)?;
        require!(price >= cfg.min_price && price <= cfg.max_price, LaunchError::PriceOutOfBand);

        // `ensurePToken`: the factory PDA is the exchange's factory signer.
        let factory_bump = [a.factory.bump];
        let factory_seeds: &[&[u8]] = &[FACTORY_SEED, &factory_bump];
        px::cpi::create_market(
            CpiContext::new_with_signer(
                px::ID,
                px::cpi::accounts::CreateMarket {
                    payer: a.creator.to_account_info(),
                    authority: a.factory.to_account_info(),
                    config: a.exchange_config.to_account_info(),
                    market: a.market.to_account_info(),
                    ptoken_mint: a.ptoken_mint.to_account_info(),
                    held: a.held.to_account_info(),
                    token_program: a.token_program.to_account_info(),
                    system_program: a.system_program.to_account_info(),
                    event_authority: a.exchange_event_authority.to_account_info(),
                    program: a.exchange_program.to_account_info(),
                },
                &[factory_seeds],
            ),
            quote.position_id,
        )?;
        // The oracle side of `verify`: an open market only.
        let m = {
            let data = a.market.try_borrow_data()?;
            px::state::Market::try_deserialize(&mut &data[..])?
        };
        require!(m.position_id == quote.position_id, LaunchError::QuoteForOtherMarket);
        require_keys_eq!(m.mint, a.ptoken_mint.key(), LaunchError::WrongAccount);
        require!(!m.settled(t), LaunchError::MarketSettled);
        require!(!m.paused, LaunchError::MarketPaused);

        // The curve, from the price and gradUsd alone.
        let c = curve::curve(a.factory.grad_usd, price).ok_or(LaunchError::MathOverflow)?;
        let base_mint = a.base_mint.key();
        let config_bump = [ctx.bumps.dbc_config];
        let config_seeds: &[&[u8]] = &[DBC_CONFIG_SEED, base_mint.as_ref(), &config_bump];
        dbc::cpi::create_config(
            CpiContext::new_with_signer(
                meteora::DBC_ID,
                dbc::cpi::accounts::CreateConfig {
                    config: a.dbc_config.to_account_info(),
                    fee_claimer: a.fee_vault.to_account_info(),
                    leftover_receiver: a.fee_vault.to_account_info(),
                    quote_mint: a.ptoken_mint.to_account_info(),
                    payer: a.creator.to_account_info(),
                    system_program: a.system_program.to_account_info(),
                    event_authority: a.dbc_event_authority.to_account_info(),
                    program: a.dbc_program.to_account_info(),
                },
                &[config_seeds],
            ),
            meteora::config_parameters(&c, &a.factory.fees),
        )?;
        let ptoken_mint = a.ptoken_mint.key();
        let fee_vault = a.fee_vault.key();
        let asked = meteora::config_parameters(&c, &a.factory.fees);
        meteora::check_config(&a.dbc_config.to_account_info(), &ptoken_mint, &fee_vault, &c, &asked)?;

        // Only the mint's holder (it signed) can create this pool on this config.
        let dbc_config = a.dbc_config.key();
        let pool = meteora::pool_address(&dbc_config, &base_mint, &ptoken_mint);

        let l = &mut a.launch;
        l.base_mint = base_mint;
        l.position_id = quote.position_id;
        l.ptoken_mint = ptoken_mint;
        l.dbc_config = dbc_config;
        l.pool = pool;
        l.pool_deadline = quote.valid_until;
        l.creator = creator;
        l.payee = creator;
        l.holders_bps = holders_bps;
        l.fees = a.factory.fees;
        l.price = price;
        l.phantom = c.phantom;
        l.threshold = c.threshold;
        l.launched_at = t;
        l.bump = ctx.bumps.launch;

        if first_buy > 0 {
            let missing = || error!(LaunchError::MissingAccount);
            let buyer_usdc = a.buyer_usdc.as_ref().ok_or_else(missing)?;
            let to = a.creator_ptoken.as_ref().ok_or_else(missing)?;
            let vault = a.usdc_vault.as_ref().ok_or_else(missing)?;
            let usdc_mint = a.usdc_mint.as_ref().ok_or_else(missing)?;
            let fill = a.fill.as_ref().ok_or_else(missing)?;
            let ata = a.associated_token_program.as_ref().ok_or_else(missing)?;
            require_keys_eq!(to.key(), get_associated_token_address(&creator, &ptoken_mint), LaunchError::WrongAccount);
            // The market may be new in this instruction, so its pToken
            // account cannot exist before it.
            anchor_spl::associated_token::create_idempotent(CpiContext::new(
                ata.key(),
                anchor_spl::associated_token::Create {
                    payer: a.creator.to_account_info(),
                    associated_token: to.to_account_info(),
                    authority: a.creator.to_account_info(),
                    mint: a.ptoken_mint.to_account_info(),
                    system_program: a.system_program.to_account_info(),
                    token_program: a.token_program.to_account_info(),
                },
            ))?;
            // The exchange verifies the same quote again (the precompile
            // instruction before this top-level one) and counts the fill.
            px::cpi::mint(
                CpiContext::new(
                    px::ID,
                    px::cpi::accounts::MintQuoted {
                        buyer: a.creator.to_account_info(),
                        config: a.exchange_config.to_account_info(),
                        market: a.market.to_account_info(),
                        ptoken_mint: a.ptoken_mint.to_account_info(),
                        buyer_usdc: buyer_usdc.to_account_info(),
                        to: to.to_account_info(),
                        vault: vault.to_account_info(),
                        usdc_mint: usdc_mint.to_account_info(),
                        fill: fill.to_account_info(),
                        instructions: ixs.clone(),
                        token_program: a.token_program.to_account_info(),
                        system_program: a.system_program.to_account_info(),
                        event_authority: a.exchange_event_authority.to_account_info(),
                        program: a.exchange_program.to_account_info(),
                    },
                )
                .with_remaining_accounts(ctx.remaining_accounts.to_vec()),
                first_buy,
                first_buy_min_out,
                quote,
            )?;
        }

        emit_cpi!(Launched {
            position_id: quote.position_id,
            creator,
            base_mint,
            pool,
            dbc_config,
            ptoken_mint,
            price,
            phantom: c.phantom,
            holders_bps,
        });
        Ok(())
    }

    /* ------------------------------------------------------ graduation */

    /// Graduate a completed coin: run DBC's `migration_damm_v2` through the
    /// pinned `Hv8L…` config, unless someone already has (DBC lets anyone,
    /// and accepts no other config for our fee option), then record the
    /// DAMM v2 pool and the fee vault's position. Anyone may send the
    /// migrating call (the recorded position is then the one DBC made from
    /// the NFT mint signing it); our keeper sends it as soon as the curve
    /// completes.
    ///
    /// If someone migrated first, the call fails on its accounts and only
    /// the keeper may resend it, naming the position DBC made (the first
    /// position of DBC's migration transaction): a third party can fund,
    /// lock and hand our vault a position identical to DBC's, so a
    /// permissionless record would let it strand the real one's fees.
    /// Nothing a third party adds to the pool can block graduation: the
    /// position is checked against the liquidity DBC's migration puts in
    /// it, a number fixed at launch, never against the pool's live total.
    pub fn graduate<'info>(ctx: Context<'info, Graduate<'info>>) -> Result<()> {
        let a = &mut *ctx.accounts;
        let base_mint = a.launch.base_mint;
        let ptoken_mint = a.launch.ptoken_mint;
        let s = meteora::curve_state(&a.dbc_pool.to_account_info())?;
        require!(s.base_mint == base_mint && s.config == a.launch.dbc_config, LaunchError::WrongAccount);
        // A pool opened after the launch quote expired is not the coin.
        require!(opened_in_time(&s, &a.launch), LaunchError::PoolTooLate);
        let damm_config = a.damm_config.key();
        require_keys_eq!(a.damm_pool.key(), meteora::damm_pool_address(&damm_config, &base_mint, &ptoken_mint), LaunchError::WrongAccount);

        let migrated_here = !s.migrated;
        // The NFT mint of the position DBC makes in this call, when it does.
        let mut minted = None;
        if migrated_here {
            require!(s.quote_reserve >= a.launch.threshold, LaunchError::NotMigrated);
            let missing = || error!(LaunchError::MissingAccount);
            let nft_mint = a.position_nft_mint.as_ref().ok_or_else(missing)?;
            minted = Some(nft_mint.key());
            dbc::cpi::migration_damm_v2(
                CpiContext::new(
                    meteora::DBC_ID,
                    dbc::cpi::accounts::MigrationDammV2 {
                        virtual_pool: a.dbc_pool.to_account_info(),
                        // Deprecated and unread by DBC 0.2.1.
                        migration_metadata: a.system_program.to_account_info(),
                        config: a.dbc_config.to_account_info(),
                        pool_authority: a.dbc_pool_authority.to_account_info(),
                        pool: a.damm_pool.to_account_info(),
                        first_position_nft_mint: nft_mint.to_account_info(),
                        first_position_nft_account: a.position_nft_account.to_account_info(),
                        first_position: a.position.to_account_info(),
                        second_position_nft_mint: a.second_position_nft_mint.as_ref().map(|k| k.to_account_info()),
                        second_position_nft_account: a.second_position_nft_account.as_ref().map(|k| k.to_account_info()),
                        second_position: a.second_position.as_ref().map(|k| k.to_account_info()),
                        damm_pool_authority: a.damm_pool_authority.to_account_info(),
                        amm_program: a.damm_program.to_account_info(),
                        base_mint: a.base_mint.to_account_info(),
                        quote_mint: a.ptoken_mint.to_account_info(),
                        token_a_vault: a.damm_token_a_vault.to_account_info(),
                        token_b_vault: a.damm_token_b_vault.to_account_info(),
                        base_vault: a.dbc_base_vault.to_account_info(),
                        quote_vault: a.dbc_quote_vault.to_account_info(),
                        payer: a.payer.to_account_info(),
                        token_base_program: a.token_program.to_account_info(),
                        token_quote_program: a.token_program.to_account_info(),
                        token_2022_program: a.token_2022_program.to_account_info(),
                        damm_event_authority: a.damm_event_authority.to_account_info(),
                        system_program: a.system_program.to_account_info(),
                    },
                )
                // DBC takes the DAMM v2 config as its first remaining account.
                .with_remaining_accounts(vec![a.damm_config.to_account_info()]),
            )?;
        } else {
            // Someone else migrated. Nothing on chain tells DBC's position
            // from one a third party funded, locked and handed to our vault
            // (Token-2022 SetAuthority, as DBC hands over its own) with
            // exactly the same liquidity, even in the migration's own
            // transaction. So only the keeper may name it, taking the first
            // position from DBC's migration transaction.
            let keeper = a.factory.keeper;
            require!(keeper != Pubkey::default() && a.payer.key() == keeper, LaunchError::OnlyKeeper);
        }
        let s = meteora::curve_state(&a.dbc_pool.to_account_info())?;
        require!(s.migrated, LaunchError::NotMigrated);
        let p = meteora::damm_pool(&a.damm_pool.to_account_info())?;
        require!(p.token_a_mint == base_mint && p.token_b_mint == ptoken_mint, LaunchError::WrongAccount);

        let pos = meteora::damm_position(&a.position.to_account_info())?;
        require_keys_eq!(pos.pool, a.damm_pool.key(), LaunchError::WrongAccount);
        // When this call migrated, the position is the one DBC just made
        // from the NFT mint that signed here (cp-amm derives it from that
        // mint; this says so outright).
        if let Some(m) = minted {
            require_keys_eq!(pos.nft_mint, m, LaunchError::WrongAccount);
        }
        let nft_ai = a.position_nft_account.to_account_info();
        // A Token-2022 account starts with the classic account layout.
        require_keys_eq!(*nft_ai.owner, meteora::TOKEN_2022, LaunchError::NotLocked);
        let nft = {
            let data = nft_ai.try_borrow_data()?;
            let base = data.get(..spl_token::state::Account::LEN).ok_or(LaunchError::NotLocked)?;
            spl_token::state::Account::unpack_from_slice(base).map_err(|_| LaunchError::NotLocked)?
        };
        let vault = Pubkey::create_program_address(&[FEE_VAULT_SEED, &[a.factory.vault_bump]], &crate::ID)
            .map_err(|_| LaunchError::WrongAccount)?;
        require!(
            nft_ai.key() == meteora::position_nft_account_address(&pos.nft_mint)
                && nft.mint == pos.nft_mint
                && nft.amount == 1
                && nft.owner == vault,
            LaunchError::NotLocked
        );
        // Exactly the liquidity DBC's migration created, all permanently
        // locked: never the pool's live total, which anyone can raise by
        // adding liquidity in their own positions. This alone does not
        // prove the position is DBC's (see above); the NFT mint that signed
        // this call, or the keeper, does.
        let migration_price = meteora::migration_sqrt_price(&a.dbc_config.to_account_info())?;
        let expected = curve::migrated_liquidity(a.launch.threshold, migration_price, s.migration_fee_bps)
            .ok_or(LaunchError::MathOverflow)?;
        require!(
            pos.unlocked_liquidity == 0 && pos.vested_liquidity == 0 && pos.permanent_locked_liquidity == expected,
            LaunchError::NotLocked
        );

        let l = &mut a.launch;
        l.graduated = true;
        l.damm_config = damm_config;
        l.damm_pool = a.damm_pool.key();
        l.position = a.position.key();
        l.position_nft_account = nft_ai.key();
        l.locked_liquidity = expected;
        let (damm_pool, position) = (l.damm_pool, l.position);
        emit_cpi!(Graduated { base_mint, damm_pool, position, locked_liquidity: expected, migrated_here });
        Ok(())
    }

    /// Close the record of a launch whose pool was not opened before its
    /// quote expired (the bundle's second transaction never landed, or the
    /// creator opened the pool later on a curve sized at a stale price).
    /// Anyone; the rent goes back to the creator. Such a coin can never be
    /// launched again (its config exists), and a pool on its config is then
    /// a look-alike whose fees `sweep_fees` takes for the platform.
    pub fn expire_launch(ctx: Context<ExpireLaunch>) -> Result<()> {
        let a = &ctx.accounts;
        let l = &a.launch;
        require!(now()? > l.pool_deadline, LaunchError::NotExpired);
        let ai = a.dbc_pool.to_account_info();
        let opened = meteora::is_curve_pool(&ai);
        if opened {
            let s = meteora::curve_state(&ai)?;
            require!(!opened_in_time(&s, l), LaunchError::NotExpired);
        }
        let (base_mint, pool) = (l.base_mint, l.pool);
        emit_cpi!(LaunchExpired { base_mint, pool, opened });
        Ok(())
    }

    /* ------------------------------------------------------------ fees */

    /// `Graduator.collect` and the curve's `_payFee` together: claim the
    /// coin's pToken fees from its DBC pool and, once graduated, from its
    /// locked DAMM v2 position; split each by the coin's creator share;
    /// credit the payee and the platform, and keep the holders' part on the
    /// record. Only this collect's balance change counts, so other coins'
    /// fees in the shared vault account and donations are never touched.
    pub fn collect(ctx: Context<Collect>) -> Result<()> {
        let a = &mut *ctx.accounts;
        // A pool opened after the launch quote expired is not the coin.
        let s = meteora::curve_state(&a.dbc_pool.to_account_info())?;
        require!(opened_in_time(&s, &a.launch), LaunchError::PoolTooLate);
        let bump = [a.factory.vault_bump];
        let seeds: &[&[u8]] = &[FEE_VAULT_SEED, &bump];
        let before = a.vault_ptoken.amount;
        dbc::cpi::claim_trading_fee(
            CpiContext::new_with_signer(
                meteora::DBC_ID,
                dbc::cpi::accounts::ClaimTradingFee {
                    pool_authority: a.dbc_pool_authority.to_account_info(),
                    config: a.dbc_config.to_account_info(),
                    pool: a.dbc_pool.to_account_info(),
                    token_a_account: a.vault_coin.to_account_info(),
                    token_b_account: a.vault_ptoken.to_account_info(),
                    base_vault: a.dbc_base_vault.to_account_info(),
                    quote_vault: a.dbc_quote_vault.to_account_info(),
                    base_mint: a.base_mint.to_account_info(),
                    quote_mint: a.ptoken_mint.to_account_info(),
                    fee_claimer: a.fee_vault.to_account_info(),
                    token_base_program: a.token_program.to_account_info(),
                    token_quote_program: a.token_program.to_account_info(),
                    event_authority: a.dbc_event_authority.to_account_info(),
                    program: a.dbc_program.to_account_info(),
                },
                &[seeds],
            ),
            u64::MAX,
            u64::MAX,
        )?;
        a.vault_ptoken.reload()?;
        let mid = a.vault_ptoken.amount;
        let curve_fees = mid.checked_sub(before).ok_or(LaunchError::MathOverflow)?;

        let mut pool_fees = 0;
        if a.launch.graduated {
            let missing = || error!(LaunchError::MissingAccount);
            cp_amm::cpi::claim_position_fee(CpiContext::new_with_signer(
                meteora::DAMM_ID,
                cp_amm::cpi::accounts::ClaimPositionFee {
                    pool_authority: a.damm_pool_authority.as_ref().ok_or_else(missing)?.to_account_info(),
                    pool: a.damm_pool.as_ref().ok_or_else(missing)?.to_account_info(),
                    position: a.position.as_ref().ok_or_else(missing)?.to_account_info(),
                    token_a_account: a.vault_coin.to_account_info(),
                    token_b_account: a.vault_ptoken.to_account_info(),
                    token_a_vault: a.damm_token_a_vault.as_ref().ok_or_else(missing)?.to_account_info(),
                    token_b_vault: a.damm_token_b_vault.as_ref().ok_or_else(missing)?.to_account_info(),
                    token_a_mint: a.base_mint.to_account_info(),
                    token_b_mint: a.ptoken_mint.to_account_info(),
                    position_nft_account: a.position_nft_account.as_ref().ok_or_else(missing)?.to_account_info(),
                    signer: a.fee_vault.to_account_info(),
                    token_a_program: a.token_program.to_account_info(),
                    token_b_program: a.token_program.to_account_info(),
                    event_authority: a.damm_event_authority.as_ref().ok_or_else(missing)?.to_account_info(),
                    program: a.damm_program.as_ref().ok_or_else(missing)?.to_account_info(),
                },
                &[seeds],
            ))?;
            a.vault_ptoken.reload()?;
            pool_fees = a.vault_ptoken.amount.checked_sub(mid).ok_or(LaunchError::MathOverflow)?;
        }

        let l = &mut a.launch;
        let c = split(curve_fees, l.fees.curve_creator_share_bps, l.holders_bps);
        let p = split(pool_fees, l.fees.pool_creator_share_bps, l.holders_bps);
        let to_payee = c.payee + p.payee;
        let to_holders = c.holders + p.holders;
        let to_platform = c.platform + p.platform;
        let mint = a.ptoken_mint.key();
        credit(&mut a.payee_owed, mint, l.payee, false, ctx.bumps.payee_owed, to_payee)?;
        credit(&mut a.platform_owed, mint, a.factory.platform, true, ctx.bumps.platform_owed, to_platform)?;
        l.holders_owed = l.holders_owed.checked_add(to_holders).ok_or(LaunchError::MathOverflow)?;
        l.curve_fees = l.curve_fees.checked_add(curve_fees).ok_or(LaunchError::MathOverflow)?;
        l.pool_fees = l.pool_fees.checked_add(pool_fees).ok_or(LaunchError::MathOverflow)?;
        let (base_mint, payee) = (l.base_mint, l.payee);
        emit_cpi!(FeesCollected { base_mint, curve: curve_fees, pool: pool_fees, payee, to_payee, to_holders, to_platform });
        Ok(())
    }

    /// Claim the partner fees DBC holds for our vault on a pool that is not
    /// a Polypad coin, for the platform: anyone can open more pools on a
    /// coin's config, or make a config naming our vault, and their trades
    /// pay us partner fees that `collect` never touches (it claims only the
    /// pool in a `Launch` record). Keeper only. A pool whose base or quote
    /// mint has a `Launch` record is refused: a coin's own pool so its fees
    /// always go through its split (an expired launch's record must be
    /// closed first), and a look-alike quoted in a coin so nothing is ever
    /// owed in a coin's mint, whose vault balance `burn_leftover` burns.
    pub fn sweep_fees(ctx: Context<SweepFees>) -> Result<()> {
        let a = &mut *ctx.accounts;
        require!(a.launch.data_is_empty() && a.quote_launch.data_is_empty(), LaunchError::IsCoin);
        let s = meteora::curve_state(&a.dbc_pool.to_account_info())?;
        require_keys_eq!(s.base_mint, a.base_mint.key(), LaunchError::WrongAccount);
        let bump = [a.factory.vault_bump];
        let seeds: &[&[u8]] = &[FEE_VAULT_SEED, &bump];
        let (base_before, quote_before) = (a.vault_base.amount, a.vault_quote.amount);
        dbc::cpi::claim_trading_fee(
            CpiContext::new_with_signer(
                meteora::DBC_ID,
                dbc::cpi::accounts::ClaimTradingFee {
                    pool_authority: a.dbc_pool_authority.to_account_info(),
                    config: a.dbc_config.to_account_info(),
                    pool: a.dbc_pool.to_account_info(),
                    token_a_account: a.vault_base.to_account_info(),
                    token_b_account: a.vault_quote.to_account_info(),
                    base_vault: a.dbc_base_vault.to_account_info(),
                    quote_vault: a.dbc_quote_vault.to_account_info(),
                    base_mint: a.base_mint.to_account_info(),
                    quote_mint: a.quote_mint.to_account_info(),
                    fee_claimer: a.fee_vault.to_account_info(),
                    token_base_program: a.token_program.to_account_info(),
                    token_quote_program: a.token_program.to_account_info(),
                    event_authority: a.dbc_event_authority.to_account_info(),
                    program: a.dbc_program.to_account_info(),
                },
                &[seeds],
            ),
            u64::MAX,
            u64::MAX,
        )?;
        a.vault_base.reload()?;
        a.vault_quote.reload()?;
        let base = a.vault_base.amount.checked_sub(base_before).ok_or(LaunchError::MathOverflow)?;
        let quote = a.vault_quote.amount.checked_sub(quote_before).ok_or(LaunchError::MathOverflow)?;
        let platform = a.factory.platform;
        let (base_mint, quote_mint) = (a.base_mint.key(), a.quote_mint.key());
        credit(&mut a.base_owed, base_mint, platform, true, ctx.bumps.base_owed, base)?;
        credit(&mut a.quote_owed, quote_mint, platform, true, ctx.bumps.quote_owed, quote)?;
        let pool = a.dbc_pool.key();
        emit_cpi!(FeesSwept { pool, base_mint, quote_mint, base, quote, platform });
        Ok(())
    }

    /// Claim the fees of a DAMM v2 position our vault holds in a pool that
    /// is not a Polypad coin's, for the platform. DBC's migration hands our
    /// vault the LP of any pool on a config naming it, and anyone may run
    /// that migration: on a coin's pool opened too late (it never
    /// graduates here), on another pool opened on a coin's config, or on a
    /// look-alike config's pool. Those locked positions would otherwise
    /// earn fees nobody can claim. Keeper only, like `sweep_fees`, and
    /// refused (IsCoin) when either of the pool's mints has a `Launch`
    /// record: a coin's own position goes through `collect`, and nothing is
    /// ever owed in a coin's mint. A late coin's record is closed first
    /// (`expire_launch`).
    pub fn sweep_position_fees(ctx: Context<SweepPositionFees>) -> Result<()> {
        let a = &mut *ctx.accounts;
        require!(a.launch_a.data_is_empty() && a.launch_b.data_is_empty(), LaunchError::IsCoin);
        let p = meteora::damm_pool(&a.damm_pool.to_account_info())?;
        require!(
            p.token_a_mint == a.token_a_mint.key() && p.token_b_mint == a.token_b_mint.key(),
            LaunchError::WrongAccount
        );
        let bump = [a.factory.vault_bump];
        let seeds: &[&[u8]] = &[FEE_VAULT_SEED, &bump];
        let (a_before, b_before) = (a.vault_a.amount, a.vault_b.amount);
        // cp-amm checks the position is the pool's and its NFT is the vault's.
        cp_amm::cpi::claim_position_fee(CpiContext::new_with_signer(
            meteora::DAMM_ID,
            cp_amm::cpi::accounts::ClaimPositionFee {
                pool_authority: a.damm_pool_authority.to_account_info(),
                pool: a.damm_pool.to_account_info(),
                position: a.position.to_account_info(),
                token_a_account: a.vault_a.to_account_info(),
                token_b_account: a.vault_b.to_account_info(),
                token_a_vault: a.damm_token_a_vault.to_account_info(),
                token_b_vault: a.damm_token_b_vault.to_account_info(),
                token_a_mint: a.token_a_mint.to_account_info(),
                token_b_mint: a.token_b_mint.to_account_info(),
                position_nft_account: a.position_nft_account.to_account_info(),
                signer: a.fee_vault.to_account_info(),
                token_a_program: a.token_program.to_account_info(),
                token_b_program: a.token_program.to_account_info(),
                event_authority: a.damm_event_authority.to_account_info(),
                program: a.damm_program.to_account_info(),
            },
            &[seeds],
        ))?;
        a.vault_a.reload()?;
        a.vault_b.reload()?;
        let fee_a = a.vault_a.amount.checked_sub(a_before).ok_or(LaunchError::MathOverflow)?;
        let fee_b = a.vault_b.amount.checked_sub(b_before).ok_or(LaunchError::MathOverflow)?;
        let platform = a.factory.platform;
        let (mint_a, mint_b) = (a.token_a_mint.key(), a.token_b_mint.key());
        credit(&mut a.owed_a, mint_a, platform, true, ctx.bumps.owed_a, fee_a)?;
        credit(&mut a.owed_b, mint_b, platform, true, ctx.bumps.owed_b, fee_b)?;
        let (pool, position) = (a.damm_pool.key(), a.position.key());
        emit_cpi!(PositionFeesSwept { pool, position, mint_a, mint_b, fee_a, fee_b, platform });
        Ok(())
    }

    /// `FeeVault.withdraw`: everything owed to the signer in one mint.
    pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
        let a = &mut *ctx.accounts;
        let amount = a.owed.amount;
        require!(amount > 0, LaunchError::NothingToClaim);
        a.owed.amount = 0;
        vault_transfer(
            &a.token_program.to_account_info(),
            &a.vault.to_account_info(),
            &a.mint.to_account_info(),
            a.mint.decimals,
            &a.to.to_account_info(),
            &a.fee_vault.to_account_info(),
            a.factory.vault_bump,
            amount,
        )?;
        let (mint, payee, to) = (a.mint.key(), a.payee.key(), a.to.key());
        emit_cpi!(Withdrawn { mint, payee, to, amount });
        Ok(())
    }

    /// Move up to `amount` of a coin's holder share to the payout wallet,
    /// from which the keeper pays holders by their indexed balances (D3).
    pub fn withdraw_holders(ctx: Context<WithdrawHolders>, amount: u64) -> Result<()> {
        let a = &mut *ctx.accounts;
        require!(amount > 0 && amount <= a.launch.holders_owed, LaunchError::NothingToClaim);
        a.launch.holders_owed -= amount;
        vault_transfer(
            &a.token_program.to_account_info(),
            &a.vault.to_account_info(),
            &a.mint.to_account_info(),
            a.mint.decimals,
            &a.to.to_account_info(),
            &a.fee_vault.to_account_info(),
            a.factory.vault_bump,
            amount,
        )?;
        let (base_mint, to) = (a.launch.base_mint, a.to.key());
        emit_cpi!(HoldersPaidOut { base_mint, to, amount });
        Ok(())
    }

    /// Start moving a coin's payee (the EVM `setPayee`, made two-step: a
    /// mistyped address would lose the coin's fees for good). Default cancels.
    pub fn propose_payee(ctx: Context<PayeeOnly>, payee: Pubkey) -> Result<()> {
        let l = &mut ctx.accounts.launch;
        require_keys_eq!(ctx.accounts.signer.key(), l.payee, LaunchError::OnlyPayee);
        l.pending_payee = payee;
        let (base_mint, current) = (l.base_mint, l.payee);
        emit_cpi!(PayeeProposed { base_mint, payee: current, pending_payee: payee });
        Ok(())
    }

    /// The proposed payee accepts. Fees already owed stay with the old
    /// payee (they are owed per payee, as on the EVM); new ones are theirs.
    pub fn accept_payee(ctx: Context<PayeeOnly>) -> Result<()> {
        let signer = ctx.accounts.signer.key();
        let l = &mut ctx.accounts.launch;
        require!(l.pending_payee != Pubkey::default() && signer == l.pending_payee, LaunchError::NotPendingPayee);
        let previous = l.payee;
        l.payee = signer;
        l.pending_payee = Pubkey::default();
        let base_mint = l.base_mint;
        emit_cpi!(PayeeSet { base_mint, previous, payee: signer });
        Ok(())
    }

    /// Burn the coins in the vault's coin account: DBC's leftover above the
    /// burn cap (sent here by its permissionless `withdraw_leftover`). The
    /// EVM sends its unsold remainder to DEAD; here supply really drops.
    /// Graduated coins only: DBC sends a leftover only after migration, and
    /// a coin that graduated had its record before its mint existed, so
    /// the sweeps (which refuse a mint with a record) never left the
    /// platform owed anything in it. A record made later for a mint that
    /// already existed (a look-alike pool's) never graduates, so what the
    /// platform is owed in that mint is never burned.
    pub fn burn_leftover(ctx: Context<BurnLeftover>) -> Result<()> {
        let a = &mut *ctx.accounts;
        require!(a.launch.graduated, LaunchError::NotMigrated);
        let amount = a.vault_coin.amount;
        require!(amount > 0, LaunchError::NothingToClaim);
        let bump = [a.factory.vault_bump];
        let seeds: &[&[u8]] = &[FEE_VAULT_SEED, &bump];
        token::burn(
            CpiContext::new_with_signer(
                a.token_program.key(),
                Burn {
                    mint: a.base_mint.to_account_info(),
                    from: a.vault_coin.to_account_info(),
                    authority: a.fee_vault.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        let base_mint = a.launch.base_mint;
        emit_cpi!(LeftoverBurned { base_mint, amount });
        Ok(())
    }
}
