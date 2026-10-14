//! Account lists for every instruction. Every account is typed or carries a
//! written check; PDAs are checked against their seeds (the exchange's under
//! the exchange program); Meteora accounts are pinned by address, by the
//! `Launch` record, or checked by Meteora against the pool the record names.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{Mint, Token, TokenAccount};
use polypad_exchange as px;
use px::quote::Quote;
use solana_sdk_ids::sysvar::instructions as sysvar_instructions;

use crate::error::LaunchError;
use crate::meteora::*;
use crate::state::*;
use crate::{cp_amm, dynamic_bonding_curve as dbc};

/* ---------------------------------------------------------------- setup */

#[event_cpi]
#[derive(Accounts)]
pub struct Initialize<'info> {
    /// The program's upgrade authority: nobody else can take the factory first.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(init, payer = payer, space = 8 + Factory::INIT_SPACE, seeds = [FACTORY_SEED], bump)]
    pub factory: Box<Account<'info, Factory>>,
    /// CHECK: data-less PDA; only its bump is stored.
    #[account(seeds = [FEE_VAULT_SEED], bump)]
    pub fee_vault: UncheckedAccount<'info>,
    #[account(constraint = launch_program.programdata_address()? == Some(program_data.key()) @ LaunchError::WrongAccount)]
    pub launch_program: Program<'info, crate::program::PolypadLaunch>,
    #[account(constraint = program_data.upgrade_authority_address == Some(payer.key()) @ LaunchError::OnlyOwner)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct OwnerOnly<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [FACTORY_SEED], bump = factory.bump, has_one = owner @ LaunchError::OnlyOwner)]
    pub factory: Box<Account<'info, Factory>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AcceptOwnership<'info> {
    pub pending_owner: Signer<'info>,
    #[account(
        mut, seeds = [FACTORY_SEED], bump = factory.bump,
        constraint = factory.pending_owner == pending_owner.key() @ LaunchError::NotPendingOwner,
    )]
    pub factory: Box<Account<'info, Factory>>,
}

/* --------------------------------------------------------------- launch */

/// `launch`: the exchange market (created if new), the per-coin DBC config
/// (created here), the `Launch` record, and for a first buy the accounts of
/// the exchange's `mint`. The queue claims `mint` pays first ride in
/// `remaining_accounts`, as for a direct mint.
#[event_cpi]
#[derive(Accounts)]
#[instruction(quote: Quote)]
pub struct LaunchCoin<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(seeds = [FACTORY_SEED], bump = factory.bump)]
    pub factory: Box<Account<'info, Factory>>,
    /// CHECK: the fee vault PDA: DBC fee claimer and leftover receiver.
    #[account(seeds = [FEE_VAULT_SEED], bump = factory.vault_bump)]
    pub fee_vault: UncheckedAccount<'info>,
    /// Writable for the first buy's `mint`; this program never writes it.
    #[account(mut, seeds = [px::state::CONFIG_SEED], bump = exchange_config.bump, seeds::program = px::ID)]
    pub exchange_config: Box<Account<'info, px::state::Config>>,
    /// CHECK: the quote's market, created by the exchange when new, then read.
    #[account(mut, seeds = [px::state::MARKET_SEED, quote.position_id.as_ref()], bump, seeds::program = px::ID)]
    pub market: UncheckedAccount<'info>,
    /// CHECK: the market's pToken mint: the only quote mint a coin may have.
    #[account(mut, seeds = [px::state::PTOKEN_SEED, quote.position_id.as_ref()], bump, seeds::program = px::ID)]
    pub ptoken_mint: UncheckedAccount<'info>,
    /// CHECK: the market's `held` account, created with the market.
    #[account(mut, seeds = [px::state::HELD_SEED, quote.position_id.as_ref()], bump, seeds::program = px::ID)]
    pub held: UncheckedAccount<'info>,
    /// CHECK: created by DBC `create_config` through CPI, signed by this PDA,
    /// so no one else can make a config at this address.
    #[account(mut, seeds = [DBC_CONFIG_SEED, base_mint.key().as_ref()], bump)]
    pub dbc_config: UncheckedAccount<'info>,
    /// The coin's fresh mint keypair (DBC creates the mint with it). Its
    /// signature means nobody can make a config for someone else's mint, and
    /// only its holder can create the pool this launch records.
    pub base_mint: Signer<'info>,
    #[account(init, payer = creator, space = 8 + Launch::INIT_SPACE, seeds = [LAUNCH_SEED, base_mint.key().as_ref()], bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: pinned to the Instructions sysvar; read only through the checked loaders.
    #[account(address = sysvar_instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    /// CHECK: the exchange's event authority.
    #[account(seeds = [b"__event_authority"], bump, seeds::program = px::ID)]
    pub exchange_event_authority: UncheckedAccount<'info>,
    pub exchange_program: Program<'info, px::program::PolypadExchange>,
    /// CHECK: DBC's event authority, pinned.
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    pub dbc_program: Program<'info, dbc::program::DynamicBondingCurve>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,

    // The creator's first buy (all or none). The exchange checks each of these
    // in its `mint`; this program only creates the pToken account.
    /// CHECK: checked by the exchange (USDC, owned by the creator).
    #[account(mut)]
    pub buyer_usdc: Option<UncheckedAccount<'info>>,
    /// CHECK: the creator's pToken ATA, created here if missing.
    #[account(mut)]
    pub creator_ptoken: Option<UncheckedAccount<'info>>,
    /// CHECK: checked by the exchange (its float vault).
    #[account(mut)]
    pub usdc_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: checked by the exchange (the pinned USDC mint).
    pub usdc_mint: Option<UncheckedAccount<'info>>,
    /// CHECK: checked by the exchange (the quote's fill PDA).
    #[account(mut)]
    pub fill: Option<UncheckedAccount<'info>>,
    pub associated_token_program: Option<Program<'info, AssociatedToken>>,
}

/* ------------------------------------------------------------ graduation */

/// `graduate`: once the curve completes (our keeper). Runs DBC's
/// `migration_damm_v2` through the pinned `Hv8L…` config unless someone
/// already has, then records the DAMM v2 pool and the fee vault's locked
/// position after checking both. The position NFT mints sign only when this
/// call migrates; anyone may send that. Recording someone else's migration
/// is the keeper's alone.
#[event_cpi]
#[derive(Accounts)]
pub struct Graduate<'info> {
    /// Pays the DAMM v2 accounts' rent when this call migrates; must be
    /// the keeper when it does not.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [FACTORY_SEED], bump = factory.bump)]
    pub factory: Box<Account<'info, Factory>>,
    #[account(
        mut, seeds = [LAUNCH_SEED, launch.base_mint.as_ref()], bump = launch.bump,
        constraint = !launch.graduated @ LaunchError::AlreadyGraduated,
    )]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the coin's DBC pool, read with owner and discriminator checks.
    #[account(mut, address = launch.pool @ LaunchError::WrongAccount)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: the coin's own config; DBC checks it against the pool.
    #[account(address = launch.dbc_config @ LaunchError::WrongAccount)]
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: pinned; lends the DAMM v2 rent during the migration.
    #[account(mut, address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DBC checks both vaults against the pool.
    #[account(mut)]
    pub dbc_base_vault: UncheckedAccount<'info>,
    /// CHECK: as above.
    #[account(mut)]
    pub dbc_quote_vault: UncheckedAccount<'info>,
    /// CHECK: the coin's mint (DBC burns the unsold remainder).
    #[account(mut, address = launch.base_mint @ LaunchError::WrongAccount)]
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the market's pToken mint.
    #[account(mut, address = launch.ptoken_mint @ LaunchError::WrongAccount)]
    pub ptoken_mint: UncheckedAccount<'info>,
    /// CHECK: pinned: the only DAMM v2 config a coin graduates into.
    #[account(address = DAMM_CONFIG @ LaunchError::WrongAccount)]
    pub damm_config: UncheckedAccount<'info>,
    /// CHECK: must be the pool of the coin's mints under `damm_config`.
    #[account(mut)]
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: cp-amm creates and checks both vaults.
    #[account(mut)]
    pub damm_token_a_vault: UncheckedAccount<'info>,
    /// CHECK: as above.
    #[account(mut)]
    pub damm_token_b_vault: UncheckedAccount<'info>,
    /// CHECK: the fee vault's position (DBC's first): read with owner and
    /// discriminator checks; its pool, NFT and liquidity are checked, and
    /// its NFT mint is the signing one when this call migrates.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: the position NFT's Token-2022 account; it must hold the NFT
    /// for the fee vault.
    #[account(mut)]
    pub position_nft_account: UncheckedAccount<'info>,
    /// The position NFT mint, when this call migrates (a fresh keypair).
    #[account(mut)]
    pub position_nft_mint: Option<Signer<'info>>,
    /// DBC's second position (rounding dust, the creator's), when this call
    /// migrates: DBC refuses if it is needed and missing.
    #[account(mut)]
    pub second_position_nft_mint: Option<Signer<'info>>,
    /// CHECK: cp-amm creates it.
    #[account(mut)]
    pub second_position_nft_account: Option<UncheckedAccount<'info>>,
    /// CHECK: cp-amm creates it.
    #[account(mut)]
    pub second_position: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned.
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: pinned.
    #[account(address = DAMM_EVENT_AUTHORITY)]
    pub damm_event_authority: UncheckedAccount<'info>,
    pub damm_program: Program<'info, cp_amm::program::CpAmm>,
    pub dbc_program: Program<'info, dbc::program::DynamicBondingCurve>,
    pub token_program: Program<'info, Token>,
    /// CHECK: pinned (position NFTs are Token-2022).
    #[account(address = TOKEN_2022)]
    pub token_2022_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `expire_launch`: anyone may close a record whose pool was not opened
/// before its quote expired. The rent goes back to the creator.
#[event_cpi]
#[derive(Accounts)]
pub struct ExpireLaunch<'info> {
    #[account(mut, seeds = [LAUNCH_SEED, launch.base_mint.as_ref()], bump = launch.bump, close = creator)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: receives the rent; must be the record's creator.
    #[account(mut, address = launch.creator @ LaunchError::WrongAccount)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the recorded pool: missing, or read with owner and discriminator checks.
    #[account(address = launch.pool @ LaunchError::WrongAccount)]
    pub dbc_pool: UncheckedAccount<'info>,
}

/* ------------------------------------------------------------------ fees */

/// `collect`: anyone. Claims the coin's fees from its DBC pool and, once
/// graduated, from its locked DAMM v2 position, then books the split.
#[event_cpi]
#[derive(Accounts)]
pub struct Collect<'info> {
    /// Pays the rent of any fee account created here.
    #[account(mut)]
    pub caller: Signer<'info>,
    #[account(seeds = [FACTORY_SEED], bump = factory.bump)]
    pub factory: Box<Account<'info, Factory>>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.base_mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the fee vault PDA; signs the claims.
    #[account(seeds = [FEE_VAULT_SEED], bump = factory.vault_bump)]
    pub fee_vault: UncheckedAccount<'info>,
    #[account(address = launch.ptoken_mint @ LaunchError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(address = launch.base_mint @ LaunchError::WrongAccount)]
    pub base_mint: Box<Account<'info, Mint>>,
    /// The vault's pToken account for this market, shared by its coins:
    /// fees are measured as this collect's balance change, never swept.
    #[account(
        init_if_needed, payer = caller,
        associated_token::mint = ptoken_mint, associated_token::authority = fee_vault,
        associated_token::token_program = token_program,
    )]
    pub vault_ptoken: Box<Account<'info, TokenAccount>>,
    /// The vault's coin account: the token A side of both claims (always 0
    /// with fees taken in pToken), and where DBC's leftover lands.
    #[account(
        init_if_needed, payer = caller,
        associated_token::mint = base_mint, associated_token::authority = fee_vault,
        associated_token::token_program = token_program,
    )]
    pub vault_coin: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed, payer = caller, space = 8 + Owed::INIT_SPACE,
        seeds = [OWED_SEED, ptoken_mint.key().as_ref(), launch.payee.as_ref()], bump,
    )]
    pub payee_owed: Box<Account<'info, Owed>>,
    #[account(
        init_if_needed, payer = caller, space = 8 + Owed::INIT_SPACE,
        seeds = [PLATFORM_OWED_SEED, ptoken_mint.key().as_ref(), factory.platform.as_ref()], bump,
    )]
    pub platform_owed: Box<Account<'info, Owed>>,

    /// CHECK: pinned.
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: the coin's own config.
    #[account(address = launch.dbc_config @ LaunchError::WrongAccount)]
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: the coin's own pool, never another pool on its config.
    #[account(mut, address = launch.pool @ LaunchError::WrongAccount)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: DBC checks both vaults against the pool.
    #[account(mut)]
    pub dbc_base_vault: UncheckedAccount<'info>,
    /// CHECK: as above.
    #[account(mut)]
    pub dbc_quote_vault: UncheckedAccount<'info>,
    /// CHECK: pinned.
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    pub dbc_program: Program<'info, dbc::program::DynamicBondingCurve>,

    // After graduation only: the locked DAMM v2 position.
    /// CHECK: pinned.
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: Option<UncheckedAccount<'info>>,
    /// CHECK: recorded by `graduate`.
    #[account(address = launch.damm_pool @ LaunchError::WrongAccount)]
    pub damm_pool: Option<UncheckedAccount<'info>>,
    /// CHECK: recorded by `graduate`.
    #[account(mut, address = launch.position @ LaunchError::WrongAccount)]
    pub position: Option<UncheckedAccount<'info>>,
    /// CHECK: DAMM v2 checks both vaults against the pool.
    #[account(mut)]
    pub damm_token_a_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: as above.
    #[account(mut)]
    pub damm_token_b_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: recorded by `graduate`.
    #[account(address = launch.position_nft_account @ LaunchError::WrongAccount)]
    pub position_nft_account: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned.
    #[account(address = DAMM_EVENT_AUTHORITY)]
    pub damm_event_authority: Option<UncheckedAccount<'info>>,
    pub damm_program: Option<Program<'info, cp_amm::program::CpAmm>>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

/// `sweep_fees`: the keeper claims the partner fees DBC holds for our vault
/// on a pool that is not a Polypad coin: another pool opened on a coin's
/// config, a pool on a look-alike config naming our vault, or the pool of
/// an expired launch. They are credited to the platform, in whatever the
/// pool paid them in, never a coin's mint. Keeper only: such a pool's mints
/// can be anything.
#[event_cpi]
#[derive(Accounts)]
pub struct SweepFees<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    #[account(
        seeds = [FACTORY_SEED], bump = factory.bump,
        constraint = factory.keeper != Pubkey::default() && factory.keeper == keeper.key() @ LaunchError::OnlyKeeper,
    )]
    pub factory: Box<Account<'info, Factory>>,
    /// CHECK: the fee vault PDA; signs the claim (DBC requires it to be the
    /// config's fee claimer).
    #[account(seeds = [FEE_VAULT_SEED], bump = factory.vault_bump)]
    pub fee_vault: UncheckedAccount<'info>,
    /// CHECK: `["launch", base_mint]`: must not exist, or the pool would be a
    /// coin whose fees `collect` splits.
    #[account(seeds = [LAUNCH_SEED, base_mint.key().as_ref()], bump)]
    pub launch: UncheckedAccount<'info>,
    /// CHECK: `["launch", quote_mint]`: must not exist either, so nothing is
    /// ever owed in a coin's mint.
    #[account(seeds = [LAUNCH_SEED, quote_mint.key().as_ref()], bump)]
    pub quote_launch: UncheckedAccount<'info>,
    pub base_mint: Box<Account<'info, Mint>>,
    pub quote_mint: Box<Account<'info, Mint>>,
    #[account(
        init_if_needed, payer = keeper,
        associated_token::mint = base_mint, associated_token::authority = fee_vault,
        associated_token::token_program = token_program,
    )]
    pub vault_base: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed, payer = keeper,
        associated_token::mint = quote_mint, associated_token::authority = fee_vault,
        associated_token::token_program = token_program,
    )]
    pub vault_quote: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed, payer = keeper, space = 8 + Owed::INIT_SPACE,
        seeds = [PLATFORM_OWED_SEED, base_mint.key().as_ref(), factory.platform.as_ref()], bump,
    )]
    pub base_owed: Box<Account<'info, Owed>>,
    #[account(
        init_if_needed, payer = keeper, space = 8 + Owed::INIT_SPACE,
        seeds = [PLATFORM_OWED_SEED, quote_mint.key().as_ref(), factory.platform.as_ref()], bump,
    )]
    pub quote_owed: Box<Account<'info, Owed>>,
    /// CHECK: pinned.
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DBC checks it is the pool's config and that our vault is its fee claimer.
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: read with owner and discriminator checks (its base mint); DBC checks the rest.
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: DBC checks both vaults against the pool.
    #[account(mut)]
    pub dbc_base_vault: UncheckedAccount<'info>,
    /// CHECK: as above.
    #[account(mut)]
    pub dbc_quote_vault: UncheckedAccount<'info>,
    /// CHECK: pinned.
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    pub dbc_program: Program<'info, dbc::program::DynamicBondingCurve>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

/// `sweep_position_fees`: the keeper claims the fees of a DAMM v2 position
/// our vault holds in a pool neither of whose mints is a coin's, for the
/// platform.
#[event_cpi]
#[derive(Accounts)]
pub struct SweepPositionFees<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    #[account(
        seeds = [FACTORY_SEED], bump = factory.bump,
        constraint = factory.keeper != Pubkey::default() && factory.keeper == keeper.key() @ LaunchError::OnlyKeeper,
    )]
    pub factory: Box<Account<'info, Factory>>,
    /// CHECK: the fee vault PDA; signs the claim as the position NFT's owner.
    #[account(seeds = [FEE_VAULT_SEED], bump = factory.vault_bump)]
    pub fee_vault: UncheckedAccount<'info>,
    /// CHECK: `["launch", token_a_mint]`: must not exist.
    #[account(seeds = [LAUNCH_SEED, token_a_mint.key().as_ref()], bump)]
    pub launch_a: UncheckedAccount<'info>,
    /// CHECK: `["launch", token_b_mint]`: must not exist.
    #[account(seeds = [LAUNCH_SEED, token_b_mint.key().as_ref()], bump)]
    pub launch_b: UncheckedAccount<'info>,
    pub token_a_mint: Box<Account<'info, Mint>>,
    pub token_b_mint: Box<Account<'info, Mint>>,
    #[account(
        init_if_needed, payer = keeper,
        associated_token::mint = token_a_mint, associated_token::authority = fee_vault,
        associated_token::token_program = token_program,
    )]
    pub vault_a: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed, payer = keeper,
        associated_token::mint = token_b_mint, associated_token::authority = fee_vault,
        associated_token::token_program = token_program,
    )]
    pub vault_b: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed, payer = keeper, space = 8 + Owed::INIT_SPACE,
        seeds = [PLATFORM_OWED_SEED, token_a_mint.key().as_ref(), factory.platform.as_ref()], bump,
    )]
    pub owed_a: Box<Account<'info, Owed>>,
    #[account(
        init_if_needed, payer = keeper, space = 8 + Owed::INIT_SPACE,
        seeds = [PLATFORM_OWED_SEED, token_b_mint.key().as_ref(), factory.platform.as_ref()], bump,
    )]
    pub owed_b: Box<Account<'info, Owed>>,
    /// CHECK: pinned.
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: read with owner and discriminator checks (its mints); cp-amm checks the rest.
    #[account(mut)]
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: cp-amm checks it is the pool's and that the NFT account holds its NFT.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: cp-amm checks the fee vault owns it.
    pub position_nft_account: UncheckedAccount<'info>,
    /// CHECK: cp-amm checks both vaults against the pool.
    #[account(mut)]
    pub damm_token_a_vault: UncheckedAccount<'info>,
    /// CHECK: as above.
    #[account(mut)]
    pub damm_token_b_vault: UncheckedAccount<'info>,
    /// CHECK: pinned.
    #[account(address = DAMM_EVENT_AUTHORITY)]
    pub damm_event_authority: UncheckedAccount<'info>,
    pub damm_program: Program<'info, cp_amm::program::CpAmm>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

/// A payee (or the platform) takes what it is owed in one mint.
#[event_cpi]
#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub payee: Signer<'info>,
    #[account(seeds = [FACTORY_SEED], bump = factory.bump)]
    pub factory: Box<Account<'info, Factory>>,
    /// CHECK: the fee vault PDA; signs the transfer.
    #[account(seeds = [FEE_VAULT_SEED], bump = factory.vault_bump)]
    pub fee_vault: UncheckedAccount<'info>,
    #[account(mut, seeds = [owed.seed(), owed.mint.as_ref(), payee.key().as_ref()], bump = owed.bump)]
    pub owed: Box<Account<'info, Owed>>,
    #[account(address = owed.mint @ LaunchError::WrongAccount)]
    pub mint: Box<Account<'info, Mint>>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = fee_vault, associated_token::token_program = token_program)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = mint, constraint = to.key() != vault.key() @ LaunchError::WrongAccount)]
    pub to: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

/// The keeper moves a coin's holder share to the payout wallet (D3).
#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawHolders<'info> {
    pub keeper: Signer<'info>,
    #[account(
        seeds = [FACTORY_SEED], bump = factory.bump,
        constraint = factory.keeper != Pubkey::default() && factory.keeper == keeper.key() @ LaunchError::OnlyKeeper,
    )]
    pub factory: Box<Account<'info, Factory>>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.base_mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the fee vault PDA; signs the transfer.
    #[account(seeds = [FEE_VAULT_SEED], bump = factory.vault_bump)]
    pub fee_vault: UncheckedAccount<'info>,
    #[account(address = launch.ptoken_mint @ LaunchError::WrongAccount)]
    pub mint: Box<Account<'info, Mint>>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = fee_vault, associated_token::token_program = token_program)]
    pub vault: Box<Account<'info, TokenAccount>>,
    /// Only the payout wallet's: a stolen keeper key cannot pay itself.
    #[account(
        mut, token::mint = mint, token::authority = factory.holder_payout,
        constraint = to.key() != vault.key() @ LaunchError::WrongAccount,
    )]
    pub to: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

/// `propose_payee` (by the payee) and `accept_payee` (by the proposed one).
#[event_cpi]
#[derive(Accounts)]
pub struct PayeeOnly<'info> {
    pub signer: Signer<'info>,
    #[account(seeds = [FACTORY_SEED], bump = factory.bump)]
    pub factory: Box<Account<'info, Factory>>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.base_mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
}

/// Burns the coin dust DBC sent the vault (`withdraw_leftover`). Anyone.
#[event_cpi]
#[derive(Accounts)]
pub struct BurnLeftover<'info> {
    #[account(seeds = [FACTORY_SEED], bump = factory.bump)]
    pub factory: Box<Account<'info, Factory>>,
    #[account(seeds = [LAUNCH_SEED, launch.base_mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the fee vault PDA; signs the burn.
    #[account(seeds = [FEE_VAULT_SEED], bump = factory.vault_bump)]
    pub fee_vault: UncheckedAccount<'info>,
    #[account(mut, address = launch.base_mint @ LaunchError::WrongAccount)]
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(mut, associated_token::mint = base_mint, associated_token::authority = fee_vault, associated_token::token_program = token_program)]
    pub vault_coin: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}
