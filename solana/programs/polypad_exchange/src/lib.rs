//! Polypad exchange on Solana: `PriceOracle`, `PExchange` and `PToken` in one
//! program (docs/SOLANA.md, docs/solana/EVM-PORT-SPEC.md §1-3), so the quote
//! check and the mint that relies on it share one trust boundary.
//!
//! Turns USDC into Polymarket shares (pTokens) and back, at a quote the pricer
//! signs with its ed25519 key, out of one shared USDC float. The rules are the
//! v14 contracts' rule for rule; where Solana differs (no try/catch, no
//! dynamic arrays, accounts instead of mappings) the module that handles it
//! says how. Differences a client must know:
//!
//! - Mint and redeem come in two forms each: at a quote (`mint`, `redeem`,
//!   which also price at the payout when one applies, ignoring the quote, as
//!   the EVM does) and at a payout with no quote (`mint_at_payout`,
//!   `redeem_at_payout`), so a settled market needs no signature at all.
//! - Payments go to a USDC token account. New claims and delayed-sale tickets
//!   are PDAs indexed by counters in `Config`; the client passes the next
//!   addresses and the program creates them only when needed. Queue payments
//!   take `(claim, recipient, rent_to)` triples in `remaining_accounts`;
//!   every mint and payment must pass the head claim's while the queue has
//!   one, as every EVM mint and payment runs `_payQueue(3)`. Clients pass the
//!   next three and resend on `MissingAccount`: a list fails only once every
//!   claim on it was paid or set aside by another transaction.
//! - A queued claim whose recipient cannot be paid (closed, frozen, wrong
//!   mint) is set aside on its own account for `withdraw_unclaimed`, as the
//!   EVM keeps `unclaimed[to]`.
//! - Holders can burn their own pTokens through the token program (on the
//!   EVM only the exchange could). The program counts such burns the next
//!   time it reads the supply (`Config::burned_outside`), exactly as an
//!   exchange redemption of the same shares: they never make an instruction
//!   fail, and they take off the market's risk and `total_risk` alike.
//!   Booking one changes both the market and the config, so every
//!   instruction that books one stores both: the batch instructions load
//!   their markets through `with_market`, which always writes the market
//!   back, whatever the instruction decided about it.
//! - The EVM's views are read-only instructions (`view_float`,
//!   `view_market`, `preview_mint`, `preview_redeem`) returning their value
//!   as return data, for simulation.

use anchor_lang::prelude::*;

pub mod contexts;
pub mod error;
pub mod events;
pub mod math;
pub mod quote;
pub mod state;
pub mod trade;
pub mod treasury;
pub mod views;

pub use contexts::*;
use error::ExchangeError;
use events::*;
use quote::{Quote, BUY, SELL};
use state::*;
use trade::{At, Buyer, Seller};
use treasury::{Float, Shares, Slots};

declare_id!("3SzMv182JofB9Li4xjhqT5s283kwv8TbTgJWych6hDAd");

/// The USDC mint the mainnet build pins (Circle's).
#[cfg(feature = "mainnet")]
pub const USDC_MAINNET: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

fn now() -> Result<i64> {
    Ok(Clock::get()?.unix_timestamp)
}

fn emit_ev<'info, E: anchor_lang::Event>(authority: &UncheckedAccount<'info>, e: &E) -> Result<()> {
    events::emit(authority.as_ref(), e)
}

/// Load a market passed in `remaining_accounts` (owner and type checked by
/// `Account::try_from`), with its pToken mint if `with_mint`.
fn market_at<'info>(ai: &'info AccountInfo<'info>) -> Result<Account<'info, Market>> {
    let m: Account<'info, Market> = Account::try_from(ai)?;
    let (addr, _) = Pubkey::find_program_address(&[MARKET_SEED, &m.position_id], &crate::ID);
    require_keys_eq!(addr, *ai.key, ExchangeError::WrongAccount);
    Ok(m)
}

/// Load a market passed in `remaining_accounts`, run `f` on it, and write it
/// back, always. `f` may book direct burns (`Config::burned_outside`), which
/// changes the config's `total_risk` too; the config is written at the end
/// of the instruction, so a market left unwritten would let the two drift
/// apart (a skipped report once took the same burn's risk off the total
/// once per skip). A market passed twice in one batch is one account, so
/// the second load sees what the first wrote.
fn with_market<'info, R>(ai: &'info AccountInfo<'info>, f: impl FnOnce(&mut Market) -> Result<R>) -> Result<R> {
    let mut m = market_at(ai)?;
    let r = f(&mut *m)?;
    m.exit(&crate::ID)?;
    Ok(r)
}

#[program]
pub mod polypad_exchange {
    use super::*;

    /* ------------------------------------------------------------ setup */

    /// Create the config and the float vault. Only the program's upgrade
    /// authority can, so nobody can front-run the deployment with their own
    /// owner. The USDC mint is pinned here for good.
    pub fn initialize(ctx: Context<Initialize>, owner: Pubkey, keeper: Pubkey, signer: Pubkey) -> Result<()> {
        #[cfg(feature = "mainnet")]
        require_keys_eq!(ctx.accounts.usdc_mint.key(), USDC_MAINNET, ExchangeError::WrongAccount);
        require!(owner != Pubkey::default(), ExchangeError::BadParams);
        // Prices, shares and USDC all use six decimals; the math assumes it.
        require!(ctx.accounts.usdc_mint.decimals == 6, ExchangeError::BadParams);
        let c = &mut ctx.accounts.config;
        c.owner = owner;
        c.keeper = keeper;
        c.signer = signer;
        c.usdc_mint = ctx.accounts.usdc_mint.key();
        c.vault = ctx.accounts.vault.key();
        c.buy_spread_bps = 25;
        c.sell_spread_bps = 25;
        c.settle_fee_bps = 50;
        c.min_price = 1_000;
        c.max_price = 999_000;
        c.meter_sales = true;
        c.default_max_risk = 5_000 * ONE;
        c.max_total_risk = 25_000 * ONE;
        c.outflow_floor = 5_000 * ONE;
        c.outflow_float_bps = 5_000;
        c.bump = ctx.bumps.config;
        c.vault_bump = ctx.bumps.vault;
        let (usdc_mint, vault) = (c.usdc_mint, c.vault);
        emit_ev(&ctx.accounts.event_authority, &Initialized { owner, usdc_mint, vault })?;
        emit_ev(&ctx.accounts.event_authority, &SignerSet { signer })?;
        emit_ev(&ctx.accounts.event_authority, &RolesSet { keeper, factory: Pubkey::default(), bridge_deposit: Pubkey::default() })
    }

    /* ----------------------------------------------------------- owner */

    /// Start a two-step ownership transfer. There is no renounce: a program
    /// without an owner could never be resumed or reconfigured. Default
    /// cancels a pending transfer.
    pub fn transfer_ownership(ctx: Context<OwnerOnly>, new_owner: Pubkey) -> Result<()> {
        let c = &mut ctx.accounts.config;
        c.pending_owner = new_owner;
        let owner = c.owner;
        emit_ev(&ctx.accounts.event_authority, &OwnershipTransferStarted { owner, pending_owner: new_owner })
    }

    pub fn accept_ownership(ctx: Context<AcceptOwnership>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        let previous = c.owner;
        c.owner = c.pending_owner;
        c.pending_owner = Pubkey::default();
        let owner = c.owner;
        emit_ev(&ctx.accounts.event_authority, &OwnershipTransferred { previous, owner })
    }

    /// Change the quote signer. A new signer takes effect `ROLE_DELAY` after
    /// this (via `accept_signer`), so a stolen owner key cannot start signing
    /// prices before anyone notices. Default revokes at once.
    pub fn set_signer(ctx: Context<OwnerOnly>, signer: Pubkey) -> Result<()> {
        let c = &mut ctx.accounts.config;
        if signer == Pubkey::default() {
            c.signer = Pubkey::default();
            c.pending_signer = Pubkey::default();
            return emit_ev(&ctx.accounts.event_authority, &SignerSet { signer });
        }
        c.pending_signer = signer;
        c.pending_signer_at = now()?.checked_add(ROLE_DELAY).ok_or(ExchangeError::MathOverflow)?;
        let effective_at = c.pending_signer_at;
        emit_ev(&ctx.accounts.event_authority, &SignerProposed { signer, effective_at })
    }

    /// Accept the proposed signer, after `ROLE_DELAY` and within `ACCEPT_WINDOW`
    /// of it, so a forgotten proposal cannot be accepted months later.
    pub fn accept_signer(ctx: Context<OwnerOnly>) -> Result<()> {
        let t = now()?;
        let c = &mut ctx.accounts.config;
        require!(
            c.pending_signer != Pubkey::default() && t <= c.pending_signer_at.saturating_add(ACCEPT_WINDOW),
            ExchangeError::NothingPending
        );
        require!(t >= c.pending_signer_at, ExchangeError::NotYet);
        c.signer = c.pending_signer;
        c.pending_signer = Pubkey::default();
        let signer = c.signer;
        emit_ev(&ctx.accounts.event_authority, &SignerSet { signer })
    }

    /// Set the keeper and factory. The bridge deposit can be set here only the
    /// first time; after that it changes through `propose_bridge_deposit` with
    /// a `BRIDGE_DELAY`, since it is where the float may be sent. Clearing is
    /// instant; only the very first bridge skips the delay, so a clear
    /// followed by a set cannot install a new one at once.
    pub fn set_roles(ctx: Context<OwnerOnly>, keeper: Pubkey, factory: Pubkey, bridge_deposit: Pubkey) -> Result<()> {
        let c = &mut ctx.accounts.config;
        if bridge_deposit != c.bridge_deposit {
            if bridge_deposit != Pubkey::default() {
                require!(!c.bridge_ever_set, ExchangeError::BridgeChangeDelayed);
                c.bridge_ever_set = true;
            }
            c.bridge_deposit = bridge_deposit;
        }
        c.keeper = keeper;
        c.factory = factory;
        let bridge_deposit = c.bridge_deposit;
        emit_ev(&ctx.accounts.event_authority, &RolesSet { keeper, factory, bridge_deposit })
    }

    pub fn propose_bridge_deposit(ctx: Context<OwnerOnly>, bridge_deposit: Pubkey) -> Result<()> {
        let c = &mut ctx.accounts.config;
        c.pending_bridge_deposit = bridge_deposit;
        c.pending_bridge_deposit_at = now()?.checked_add(BRIDGE_DELAY).ok_or(ExchangeError::MathOverflow)?;
        let effective_at = c.pending_bridge_deposit_at;
        emit_ev(&ctx.accounts.event_authority, &BridgeDepositProposed { bridge_deposit, effective_at })
    }

    pub fn accept_bridge_deposit(ctx: Context<OwnerOnly>) -> Result<()> {
        let t = now()?;
        let c = &mut ctx.accounts.config;
        require!(c.pending_bridge_deposit_at != 0, ExchangeError::NothingPending);
        require!(t >= c.pending_bridge_deposit_at, ExchangeError::NotYet);
        c.bridge_deposit = c.pending_bridge_deposit;
        if c.bridge_deposit != Pubkey::default() {
            c.bridge_ever_set = true;
        }
        c.pending_bridge_deposit = Pubkey::default();
        c.pending_bridge_deposit_at = 0;
        let (keeper, factory, bridge_deposit) = (c.keeper, c.factory, c.bridge_deposit);
        emit_ev(&ctx.accounts.event_authority, &RolesSet { keeper, factory, bridge_deposit })
    }

    /// Spreads at most 5%; the band inside [0.1c, $1).
    pub fn set_params(
        ctx: Context<OwnerOnly>,
        buy_spread_bps: u16,
        sell_spread_bps: u16,
        min_price: u64,
        max_price: u64,
        default_max_risk: u64,
    ) -> Result<()> {
        require!(
            buy_spread_bps <= MAX_SPREAD_BPS
                && sell_spread_bps <= MAX_SPREAD_BPS
                && min_price >= MIN_BAND_PRICE
                && min_price < max_price
                && max_price < ONE,
            ExchangeError::BadParams
        );
        let c = &mut ctx.accounts.config;
        c.buy_spread_bps = buy_spread_bps;
        c.sell_spread_bps = sell_spread_bps;
        c.min_price = min_price;
        c.max_price = max_price;
        c.default_max_risk = default_max_risk;
        emit_ev(
            &ctx.accounts.event_authority,
            &ParamsSet { buy_spread_bps, sell_spread_bps, min_price, max_price, default_max_risk },
        )
    }

    pub fn set_max_total_risk(ctx: Context<OwnerOnly>, cap: u64) -> Result<()> {
        ctx.accounts.config.max_total_risk = cap;
        emit_ev(&ctx.accounts.event_authority, &MaxTotalRiskSet { max_total_risk: cap })
    }

    pub fn set_settle_fee(ctx: Context<OwnerOnly>, settle_fee_bps: u16) -> Result<()> {
        require!(settle_fee_bps <= MAX_SETTLE_FEE_BPS, ExchangeError::BadParams);
        ctx.accounts.config.settle_fee_bps = settle_fee_bps;
        emit_ev(&ctx.accounts.event_authority, &SettleFeeSet { settle_fee_bps })
    }

    pub fn set_meter_sales(ctx: Context<OwnerOnly>, on: bool) -> Result<()> {
        ctx.accounts.config.meter_sales = on;
        emit_ev(&ctx.accounts.event_authority, &MeterSalesSet { on })
    }

    /// The hourly redemption cap: `float_bps` of the float, never below
    /// `floor`. At most 100%: a cap above the float would never bind.
    pub fn set_outflow_cap(ctx: Context<OwnerOnly>, floor: u64, float_bps: u64) -> Result<()> {
        require!(float_bps <= BPS, ExchangeError::BadParams);
        let c = &mut ctx.accounts.config;
        c.outflow_floor = floor;
        c.outflow_float_bps = float_bps;
        emit_ev(&ctx.accounts.event_authority, &OutflowCapSet { floor, float_bps })
    }

    /// Stop every mint, redeem, release and queue payment. The keeper may halt; only the owner resumes.
    pub fn halt(ctx: Context<KeeperOrOwner>) -> Result<()> {
        ctx.accounts.config.halted = true;
        emit_ev(&ctx.accounts.event_authority, &HaltSet { halted: true })
    }

    pub fn resume(ctx: Context<OwnerOnly>) -> Result<()> {
        ctx.accounts.config.halted = false;
        emit_ev(&ctx.accounts.event_authority, &HaltSet { halted: false })
    }

    /* --------------------------------------------------------- markets */

    /// The market and its pToken, created on first use (`ensurePToken`); a
    /// second call for the same market changes nothing. Factory or owner.
    pub fn create_market(ctx: Context<CreateMarket>, position_id: [u8; 32]) -> Result<()> {
        // Id 0 is no Polymarket position, and the EVM never trades it.
        require!(position_id != [0u8; 32], ExchangeError::BadParams);
        let m = &mut ctx.accounts.market;
        if m.mint != Pubkey::default() {
            return Ok(());
        }
        m.position_id = position_id;
        m.mint = ctx.accounts.ptoken_mint.key();
        m.held = ctx.accounts.held.key();
        m.bump = ctx.bumps.market;
        m.mint_bump = ctx.bumps.ptoken_mint;
        m.held_bump = ctx.bumps.held;
        let mint = m.mint;
        emit_ev(&ctx.accounts.event_authority, &PTokenCreated { position_id, mint })
    }

    /// Name a market's share after its outcome. Keeper or owner. Kept on the
    /// market account (Metaplex metadata is a later step).
    pub fn set_share_label(ctx: Context<MarketAdmin>, name: String, symbol: String) -> Result<()> {
        let who = ctx.accounts.authority.key();
        let c = &ctx.accounts.config;
        require!(c.is_keeper(&who) || c.is_owner(&who), ExchangeError::OnlyKeeper);
        require!(name.len() <= MAX_LABEL_NAME && symbol.len() <= MAX_LABEL_SYMBOL, ExchangeError::BadParams);
        let m = &mut ctx.accounts.market;
        m.name = name.clone();
        m.symbol = symbol.clone();
        let position_id = m.position_id;
        emit_ev(&ctx.accounts.event_authority, &ShareLabelSet { position_id, name, symbol })
    }

    /// A market's own risk cap, USDC; 0 = the default. Owner.
    pub fn set_max_risk(ctx: Context<MarketAdmin>, cap: u64) -> Result<()> {
        require!(ctx.accounts.config.is_owner(&ctx.accounts.authority.key()), ExchangeError::OnlyOwner);
        ctx.accounts.market.max_risk_override = cap;
        let position_id = ctx.accounts.market.position_id;
        emit_ev(&ctx.accounts.event_authority, &MaxRiskSet { position_id, cap })
    }

    /// Pause or unpause the markets in `remaining_accounts`. Keeper.
    pub fn set_paused<'info>(ctx: Context<'info, KeeperBatch<'info>>, paused: bool) -> Result<()> {
        for ai in ctx.remaining_accounts.iter() {
            // A market with a recorded payout is decided: buying never
            // reopens. Only the owner's cancel_settle lifts that pause.
            let set = with_market(ai, |m| {
                let set = paused || m.settle_at == 0;
                if set {
                    m.paused = paused;
                }
                Ok(set.then_some(m.position_id))
            })?;
            if let Some(position_id) = set {
                emit_ev(&ctx.accounts.event_authority, &PausedSet { position_id, paused })?;
            }
        }
        Ok(())
    }

    /// Record the final payout per share, 0..$1. It takes effect after
    /// `SETTLE_DELAY`, and buying stops at once. One-way once in effect.
    /// Keeper or owner; a keeper whose settlement on this market the owner
    /// cancelled may not record another there (its key may be stolen); a new
    /// keeper may.
    pub fn settle(ctx: Context<MarketAdmin>, payout: u64) -> Result<()> {
        let who = ctx.accounts.authority.key();
        let c = &ctx.accounts.config;
        let m = &mut ctx.accounts.market;
        require!(c.is_owner(&who) || (c.is_keeper(&who) && who != m.barred), ExchangeError::OnlyKeeper);
        require!(payout <= ONE, ExchangeError::PriceOutOfRange);
        require!(m.settle_at == 0, ExchangeError::AlreadySettled);
        let at = now()?.checked_add(SETTLE_DELAY).ok_or(ExchangeError::MathOverflow)?;
        m.paused_before_settle = m.paused;
        m.paused = true;
        m.payout = payout;
        m.settle_at = at;
        m.settled_by = who;
        let position_id = m.position_id;
        emit_ev(&ctx.accounts.event_authority, &PausedSet { position_id, paused: true })?;
        emit_ev(&ctx.accounts.event_authority, &Settled { position_id, payout, effective_at: at })
    }

    /// Withdraw a settlement that has not taken effect yet (a wrong or forged
    /// payout). The market goes back to how it was before it: paused only if
    /// it already was. The keeper that recorded it can no longer settle here.
    pub fn cancel_settle(ctx: Context<MarketAdmin>) -> Result<()> {
        let c = &ctx.accounts.config;
        require!(c.is_owner(&ctx.accounts.authority.key()), ExchangeError::OnlyOwner);
        let t = now()?;
        let owner = c.owner;
        let m = &mut ctx.accounts.market;
        require!(m.settle_at != 0 && t < m.settle_at, ExchangeError::NothingPending);
        m.settle_at = 0;
        m.payout = 0;
        if m.settled_by != owner {
            m.barred = m.settled_by;
        }
        m.settled_by = Pubkey::default();
        let was = m.paused_before_settle;
        m.paused_before_settle = false;
        let position_id = m.position_id;
        if !was {
            m.paused = false;
            emit_ev(&ctx.accounts.event_authority, &PausedSet { position_id, paused: false })?;
        }
        emit_ev(&ctx.accounts.event_authority, &SettleCancelled { position_id })
    }

    /// Shares the desk holds on Polymarket, per market, as it read them when
    /// each market's `burned_total` was `seen` and its `minted_total` was
    /// `minted` (one read of the market, taken before the holdings).
    /// `remaining_accounts` holds `(market, pToken mint)` pairs in the order
    /// of `amounts`. Keeper. A report against another state (a keeper bug)
    /// is skipped, not refused, so it cannot stop the rest of the batch; the
    /// market is stored all the same, as the visit booked any direct burns
    /// into it and into `total_risk` alike.
    pub fn report_backed<'info>(
        ctx: Context<'info, KeeperBatch<'info>>,
        amounts: Vec<u64>,
        seen: Vec<u64>,
        minted: Vec<u64>,
    ) -> Result<()> {
        let pairs = ctx.remaining_accounts;
        require!(
            amounts.len() == seen.len() && amounts.len() == minted.len() && pairs.len() == amounts.len() * 2,
            ExchangeError::BadParams
        );
        let t = now()?;
        for (i, pair) in pairs.chunks_exact(2).enumerate() {
            let config = &mut ctx.accounts.config;
            let reported = with_market(&pair[0], |m| {
                require_keys_eq!(*pair[1].key, m.mint, ExchangeError::WrongAccount);
                let supply = treasury::mint_supply(&pair[1])?;
                let ok = config.report(m, supply, amounts[i], seen[i], minted[i], t)?;
                Ok(ok.then_some((m.position_id, m.backed)))
            })?;
            if let Some((position_id, amount)) = reported {
                emit_ev(&ctx.accounts.event_authority, &BackedReported { position_id, amount })?;
            }
        }
        Ok(())
    }

    /// Bring markets' stored risk down to what their unbacked supply can carry
    /// now (or to 0 once settled). Only ever lowers it; anyone may call.
    /// `remaining_accounts` holds `(market, pToken mint)` pairs.
    pub fn refresh_risk<'info>(ctx: Context<'info, AnyoneBatch<'info>>) -> Result<()> {
        let t = now()?;
        for pair in ctx.remaining_accounts.chunks_exact(2) {
            let config = &mut ctx.accounts.config;
            with_market(&pair[0], |m| {
                require_keys_eq!(*pair[1].key, m.mint, ExchangeError::WrongAccount);
                let supply = treasury::mint_supply(&pair[1])?;
                config.refresh(m, supply, t)
            })?;
        }
        Ok(())
    }

    /* ----------------------------------------------------------- trade */

    /// Pay USDC, receive pToken at the quoted buy price plus the buy spread;
    /// or at the payout plus the spread when one applies (quote ignored).
    /// The transaction's previous instruction must be the ed25519
    /// verification of `quote` by the oracle signer.
    pub fn mint<'info>(ctx: Context<'info, MintQuoted<'info>>, usdc_in: u64, min_out: u64, quote: Quote) -> Result<()> {
        require!(usdc_in > 0, ExchangeError::ZeroAmount);
        let t = now()?;
        let a = &mut *ctx.accounts;
        trade::open_fill(&mut a.fill, ctx.bumps.fill, a.buyer.key(), &quote);
        let (at, price) = match trade::payout_mint(&a.market, t) {
            Some(at) => (at, a.market.payout),
            None => (At::Quote, trade::quoted(&a.config, &a.market, &mut a.fill, &a.instructions, &quote, BUY, usdc_in, t)?),
        };
        let shares = Shares {
            authority: a.config.to_account_info(),
            bump: a.config.bump,
            mint: a.ptoken_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
        };
        let who = Buyer {
            buyer: a.buyer.to_account_info(),
            buyer_usdc: a.buyer_usdc.to_account_info(),
            to: a.to.to_account_info(),
        };
        let mut f = Float {
            authority: a.config.to_account_info(),
            vault: a.vault.to_account_info(),
            usdc_mint: a.usdc_mint.to_account_info(),
            usdc_decimals: a.usdc_mint.decimals,
            token_program: a.token_program.to_account_info(),
            event_authority: a.event_authority.to_account_info(),
            closes: Vec::new(),
            config: &mut a.config,
        };
        trade::buy(&mut f, &mut a.market, &shares, &who, usdc_in, min_out, price, at, t, ctx.remaining_accounts)?;
        f.finish()
    }

    /// A mint with no quote: the market is settled at a payout, or has a $1
    /// payout recorded and still inside its delay. A $0 payout refuses.
    pub fn mint_at_payout<'info>(ctx: Context<'info, MintAtPayout<'info>>, usdc_in: u64, min_out: u64) -> Result<()> {
        require!(usdc_in > 0, ExchangeError::ZeroAmount);
        let t = now()?;
        let a = &mut *ctx.accounts;
        let at = trade::payout_mint(&a.market, t).ok_or(ExchangeError::NotAtPayout)?;
        let price = a.market.payout;
        let shares = Shares {
            authority: a.config.to_account_info(),
            bump: a.config.bump,
            mint: a.ptoken_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
        };
        let who = Buyer {
            buyer: a.buyer.to_account_info(),
            buyer_usdc: a.buyer_usdc.to_account_info(),
            to: a.to.to_account_info(),
        };
        let mut f = Float {
            authority: a.config.to_account_info(),
            vault: a.vault.to_account_info(),
            usdc_mint: a.usdc_mint.to_account_info(),
            usdc_decimals: a.usdc_mint.decimals,
            token_program: a.token_program.to_account_info(),
            event_authority: a.event_authority.to_account_info(),
            closes: Vec::new(),
            config: &mut a.config,
        };
        trade::buy(&mut f, &mut a.market, &shares, &who, usdc_in, min_out, price, at, t, ctx.remaining_accounts)?;
        f.finish()
    }

    /// Burn pToken, receive USDC at the quoted sell price minus the sell
    /// spread, or at the payout less the settle fee once the market has
    /// settled (quote ignored). Works on a paused market.
    pub fn redeem<'info>(ctx: Context<'info, RedeemQuoted<'info>>, amount_in: u64, min_out: u64, quote: Quote) -> Result<()> {
        require!(amount_in > 0, ExchangeError::ZeroAmount);
        let t = now()?;
        let a = &mut *ctx.accounts;
        trade::open_fill(&mut a.fill, ctx.bumps.fill, a.seller.key(), &quote);
        let settled = a.market.settled(t);
        let price = if settled {
            a.market.payout
        } else {
            trade::quoted(&a.config, &a.market, &mut a.fill, &a.instructions, &quote, SELL, amount_in, t)?
        };
        let shares = Shares {
            authority: a.config.to_account_info(),
            bump: a.config.bump,
            mint: a.ptoken_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
        };
        let mut slots = Slots {
            payer: a.seller.to_account_info(),
            system_program: a.system_program.to_account_info(),
            claim: a.new_claim.as_ref().map(|s| s.as_ref()),
            aside: None,
        };
        let who = Seller {
            seller: a.seller.to_account_info(),
            seller_ptoken: a.seller_ptoken.to_account_info(),
            held: a.held.to_account_info(),
            to: a.to.to_account_info(),
            to_owner: a.to.owner,
            new_ticket: a.new_ticket.as_ref().map(|s| s.as_ref()),
        };
        let mut f = Float {
            authority: a.config.to_account_info(),
            vault: a.vault.to_account_info(),
            usdc_mint: a.usdc_mint.to_account_info(),
            usdc_decimals: a.usdc_mint.decimals,
            token_program: a.token_program.to_account_info(),
            event_authority: a.event_authority.to_account_info(),
            closes: Vec::new(),
            config: &mut a.config,
        };
        trade::sell(&mut f, &mut a.market, &shares, &who, &mut slots, amount_in, min_out, price, settled, t, ctx.remaining_accounts)?;
        f.finish()
    }

    /// Redeem a settled market at its payout less the settle fee. No quote.
    pub fn redeem_at_payout<'info>(ctx: Context<'info, RedeemAtPayout<'info>>, amount_in: u64, min_out: u64) -> Result<()> {
        require!(amount_in > 0, ExchangeError::ZeroAmount);
        let t = now()?;
        let a = &mut *ctx.accounts;
        require!(a.market.settled(t), ExchangeError::NotAtPayout);
        let price = a.market.payout;
        let shares = Shares {
            authority: a.config.to_account_info(),
            bump: a.config.bump,
            mint: a.ptoken_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
        };
        let mut slots = Slots {
            payer: a.seller.to_account_info(),
            system_program: a.system_program.to_account_info(),
            claim: a.new_claim.as_ref().map(|s| s.as_ref()),
            aside: None,
        };
        let who = Seller {
            seller: a.seller.to_account_info(),
            seller_ptoken: a.seller_ptoken.to_account_info(),
            held: a.held.to_account_info(),
            to: a.to.to_account_info(),
            to_owner: a.to.owner,
            new_ticket: a.new_ticket.as_ref().map(|s| s.as_ref()),
        };
        let mut f = Float {
            authority: a.config.to_account_info(),
            vault: a.vault.to_account_info(),
            usdc_mint: a.usdc_mint.to_account_info(),
            usdc_decimals: a.usdc_mint.decimals,
            token_program: a.token_program.to_account_info(),
            event_authority: a.event_authority.to_account_info(),
            closes: Vec::new(),
            config: &mut a.config,
        };
        trade::sell(&mut f, &mut a.market, &shares, &who, &mut slots, amount_in, min_out, price, true, t, ctx.remaining_accounts)?;
        f.finish()
    }

    /* ----------------------------------------------------------- float */

    /// Burn pToken the exchange holds beyond delayed-sale stock: in `held`,
    /// and in `other` if passed (the config PDA's associated token account,
    /// or any pToken account it owns). Anyone.
    pub fn absorb(ctx: Context<Absorb>) -> Result<()> {
        let t = now()?;
        let a = &mut *ctx.accounts;
        let shares = Shares {
            authority: a.config.to_account_info(),
            bump: a.config.bump,
            mint: a.ptoken_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
        };
        let held = a.held.to_account_info();
        let other = a.other.as_ref().map(|o| o.to_account_info());
        let events = a.event_authority.to_account_info();
        trade::absorb(&mut a.config, &mut a.market, &shares, &held, other.as_ref(), &events, t)?;
        Ok(())
    }

    /// Pay up to `max` queued claims, oldest first, while the float covers
    /// them. Anyone may call.
    pub fn pay_queue<'info>(ctx: Context<'info, PayQueue<'info>>, max: u8) -> Result<()> {
        let a = &mut *ctx.accounts;
        require!(!a.config.halted, ExchangeError::Halted);
        let mut f = Float {
            authority: a.config.to_account_info(),
            vault: a.vault.to_account_info(),
            usdc_mint: a.usdc_mint.to_account_info(),
            usdc_decimals: a.usdc_mint.decimals,
            token_program: a.token_program.to_account_info(),
            event_authority: a.event_authority.to_account_info(),
            closes: Vec::new(),
            config: &mut a.config,
        };
        f.pay_queue((max as usize).min(MAX_PAY_QUEUE), ctx.remaining_accounts, false)?;
        f.finish()
    }

    /// Pay a set-aside claim to the account it was owed to. Anyone may call,
    /// for a recipient that cannot call itself; not stopped by `halt`.
    pub fn withdraw_unclaimed(ctx: Context<WithdrawUnclaimed>) -> Result<()> {
        let a = &mut *ctx.accounts;
        let amount = a.claim.amount;
        require!(amount > 0, ExchangeError::ZeroAmount);
        a.config.unclaimed_total = a.config.unclaimed_total.checked_sub(amount).ok_or(ExchangeError::MathOverflow)?;
        a.config.queued = a.config.queued.checked_sub(amount).ok_or(ExchangeError::MathOverflow)?;
        let f = Float {
            authority: a.config.to_account_info(),
            vault: a.vault.to_account_info(),
            usdc_mint: a.usdc_mint.to_account_info(),
            usdc_decimals: a.usdc_mint.decimals,
            token_program: a.token_program.to_account_info(),
            event_authority: a.event_authority.to_account_info(),
            closes: Vec::new(),
            config: &mut a.config,
        };
        f.send(&a.to.to_account_info(), amount)?;
        f.emit(&UnclaimedWithdrawn { claim: a.claim.key(), to: a.to.key(), amount })
    }

    /// Pay a delayed sale once `DELAY` has passed. Anyone may call.
    pub fn release<'info>(ctx: Context<'info, Release<'info>>) -> Result<()> {
        let t = now()?;
        let a = &mut *ctx.accounts;
        let ticket: DelayedSale = (**a.ticket).clone();
        let shares = Shares {
            authority: a.config.to_account_info(),
            bump: a.config.bump,
            mint: a.ptoken_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
        };
        let mut slots = Slots {
            payer: a.caller.to_account_info(),
            system_program: a.system_program.to_account_info(),
            claim: a.new_claim.as_ref().map(|s| s.as_ref()),
            aside: a.new_aside.as_ref().map(|s| s.as_ref()),
        };
        let held = a.held.to_account_info();
        let to = a.to.to_account_info();
        let mut f = Float {
            authority: a.config.to_account_info(),
            vault: a.vault.to_account_info(),
            usdc_mint: a.usdc_mint.to_account_info(),
            usdc_decimals: a.usdc_mint.decimals,
            token_program: a.token_program.to_account_info(),
            event_authority: a.event_authority.to_account_info(),
            closes: Vec::new(),
            config: &mut a.config,
        };
        trade::release(&mut f, &mut a.market, &shares, &held, &ticket, &to, &mut slots, t, ctx.remaining_accounts)?;
        f.finish()
    }

    /// Undo a delayed sale before it is released. Owner; works while halted.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let a = &mut *ctx.accounts;
        let ticket: DelayedSale = (**a.ticket).clone();
        let shares = Shares {
            authority: a.config.to_account_info(),
            bump: a.config.bump,
            mint: a.ptoken_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
        };
        let (held, dest, events) = (a.held.to_account_info(), a.dest.to_account_info(), a.event_authority.to_account_info());
        trade::cancel(&mut a.market, &shares, &held, &dest, &ticket, &events)
    }

    /// Move float to the desk's Polymarket deposit account, never more than
    /// the free float (USDC owed to the queue stays). Keeper; not stopped by
    /// `halt`, so the desk can still be funded to pay the queue.
    pub fn send_to_bridge(ctx: Context<SendToBridge>, amount: u64) -> Result<()> {
        let a = &mut *ctx.accounts;
        let free = a.config.free_float(a.vault.amount);
        require!(amount <= free, ExchangeError::InsufficientFloat);
        let bump = [a.config.bump];
        let seeds: &[&[u8]] = &[CONFIG_SEED, &bump];
        anchor_spl::token::transfer_checked(
            CpiContext::new_with_signer(
                a.token_program.key(),
                anchor_spl::token::TransferChecked {
                    from: a.vault.to_account_info(),
                    mint: a.usdc_mint.to_account_info(),
                    to: a.bridge.to_account_info(),
                    authority: a.config.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            a.usdc_mint.decimals,
        )?;
        emit_ev(&a.event_authority, &SentToBridge { to: a.bridge.key(), amount })
    }

    /// Return tokens sent to the exchange by mistake. Never USDC or a pToken.
    pub fn rescue(ctx: Context<Rescue>, amount: u64) -> Result<()> {
        let a = &mut *ctx.accounts;
        let bump = [a.config.bump];
        let seeds: &[&[u8]] = &[CONFIG_SEED, &bump];
        anchor_spl::token::transfer_checked(
            CpiContext::new_with_signer(
                a.token_program.key(),
                anchor_spl::token::TransferChecked {
                    from: a.source.to_account_info(),
                    mint: a.mint.to_account_info(),
                    to: a.dest.to_account_info(),
                    authority: a.config.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            a.mint.decimals,
        )?;
        emit_ev(&a.event_authority, &Rescued { mint: a.mint.key(), to: a.dest.key(), amount })
    }

    /// Close an expired quote's fill record; its rent goes back to whoever
    /// paid it. Anyone, once the quote is past `valid_until + MAX_VALIDITY`:
    /// it can never verify again, so its fill can never be needed.
    pub fn close_fill(ctx: Context<CloseFill>) -> Result<()> {
        let t = now()?;
        require!(
            t > ctx.accounts.fill.valid_until.saturating_add(quote::MAX_VALIDITY),
            ExchangeError::FillLive
        );
        Ok(())
    }

    /* ----------------------------------------------------------- views */

    /// The float and the queue now (`freeFloat`, `outflowRemaining`,
    /// `queueLength`, `delayedLength`). Read-only: call it in a simulation.
    pub fn view_float(ctx: Context<ViewFloat>) -> Result<views::FloatView> {
        Ok(views::float_view(&ctx.accounts.config, ctx.accounts.vault.amount, now()?))
    }

    /// One market now (`oracle.status`, `oracle.settlement`, `maxRisk`, the
    /// supply and backing, and the `seen` a keeper reports against). Read-only.
    pub fn view_market(ctx: Context<ViewMarket>) -> Result<views::MarketView> {
        let mut c = (**ctx.accounts.config).clone();
        let mut m = (**ctx.accounts.market).clone();
        Ok(views::market_view(&mut c, &mut m, ctx.accounts.ptoken_mint.supply, now()?))
    }

    /// pToken a mint of `usdc_in` at `price` would pay (`mintOut`). Read-only.
    pub fn preview_mint(ctx: Context<ViewConfig>, price: u64, usdc_in: u64) -> Result<u64> {
        views::preview_mint(&ctx.accounts.config, price, usdc_in)
    }

    /// USDC a redemption of `amount_in` at `price` would pay, at a quote or
    /// (`settled`) at a payout (`redeemOut`). Before any outflow cap or queue. Read-only.
    pub fn preview_redeem(ctx: Context<ViewConfig>, price: u64, amount_in: u64, settled: bool) -> Result<u64> {
        views::preview_redeem(&ctx.accounts.config, price, amount_in, settled)
    }
}
