//! Account lists for every instruction. Every account is typed or carries a
//! written check; every PDA is checked against its seeds and stored bump;
//! token accounts are checked for mint and owner; paired mutable accounts of
//! the same mint carry a key-inequality constraint.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program_option::COption;
use anchor_spl::token::{Mint, Token, TokenAccount};
use solana_sdk_ids::sysvar::instructions as sysvar_instructions;

use crate::error::ExchangeError;
use crate::quote::Quote;
use crate::state::*;

/* ---------------------------------------------------------------- setup */

#[event_cpi]
#[derive(Accounts)]
pub struct Initialize<'info> {
    /// The program's upgrade authority: nobody else can take the config first.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(init, payer = payer, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        init, payer = payer, seeds = [VAULT_SEED], bump,
        token::mint = usdc_mint, token::authority = config, token::token_program = token_program,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(constraint = exchange_program.programdata_address()? == Some(program_data.key()) @ ExchangeError::WrongAccount)]
    pub exchange_program: Program<'info, crate::program::PolypadExchange>,
    #[account(constraint = program_data.upgrade_authority_address == Some(payer.key()) @ ExchangeError::OnlyOwner)]
    pub program_data: Account<'info, ProgramData>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// Owner-only setters.
#[event_cpi]
#[derive(Accounts)]
pub struct OwnerOnly<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = owner @ ExchangeError::OnlyOwner)]
    pub config: Box<Account<'info, Config>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AcceptOwnership<'info> {
    pub pending_owner: Signer<'info>,
    #[account(
        mut, seeds = [CONFIG_SEED], bump = config.bump,
        constraint = config.pending_owner == pending_owner.key() @ ExchangeError::NotPendingOwner,
    )]
    pub config: Box<Account<'info, Config>>,
}

/// `halt`: the keeper or the owner.
#[event_cpi]
#[derive(Accounts)]
pub struct KeeperOrOwner<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut, seeds = [CONFIG_SEED], bump = config.bump,
        constraint = config.is_keeper(&authority.key()) || config.is_owner(&authority.key()) @ ExchangeError::OnlyKeeper,
    )]
    pub config: Box<Account<'info, Config>>,
}

/// Keeper batches over markets passed in `remaining_accounts`.
#[event_cpi]
#[derive(Accounts)]
pub struct KeeperBatch<'info> {
    pub keeper: Signer<'info>,
    #[account(
        mut, seeds = [CONFIG_SEED], bump = config.bump,
        constraint = config.is_keeper(&keeper.key()) @ ExchangeError::OnlyKeeper,
    )]
    pub config: Box<Account<'info, Config>>,
}

/// `refresh_risk`: anyone, markets in `remaining_accounts`.
#[derive(Accounts)]
pub struct AnyoneBatch<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
}

/* -------------------------------------------------------------- markets */

#[event_cpi]
#[derive(Accounts)]
#[instruction(position_id: [u8; 32])]
pub struct CreateMarket<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The factory (the launch program's signer) or the owner.
    #[account(constraint = authority.key() == config.factory || authority.key() == config.owner @ ExchangeError::OnlyFactory)]
    pub authority: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        init_if_needed, payer = payer, space = 8 + Market::INIT_SPACE,
        seeds = [MARKET_SEED, position_id.as_ref()], bump,
    )]
    pub market: Box<Account<'info, Market>>,
    /// Classic SPL Token, 6 decimals, minted only by the exchange PDA, no freeze authority.
    #[account(
        init_if_needed, payer = payer, seeds = [PTOKEN_SEED, position_id.as_ref()], bump,
        mint::decimals = PTOKEN_DECIMALS, mint::authority = config, mint::token_program = token_program,
        constraint = ptoken_mint.freeze_authority == COption::None @ ExchangeError::WrongAccount,
    )]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(
        init_if_needed, payer = payer, seeds = [HELD_SEED, position_id.as_ref()], bump,
        token::mint = ptoken_mint, token::authority = config, token::token_program = token_program,
    )]
    pub held: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// Per-market keeper or owner actions; the handler checks which role.
#[event_cpi]
#[derive(Accounts)]
pub struct MarketAdmin<'info> {
    pub authority: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, market.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
}

/* ---------------------------------------------------------------- trade */

#[event_cpi]
#[derive(Accounts)]
#[instruction(usdc_in: u64, min_out: u64, quote: Quote)]
pub struct MintQuoted<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, market.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(mut, address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(
        mut, token::mint = usdc_mint, token::authority = buyer,
        constraint = buyer_usdc.key() != vault.key() @ ExchangeError::WrongAccount,
    )]
    pub buyer_usdc: Box<Account<'info, TokenAccount>>,
    /// Receives the pToken.
    #[account(mut, token::mint = ptoken_mint)]
    pub to: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(
        init_if_needed, payer = buyer, space = 8 + QuoteFill::INIT_SPACE,
        seeds = [FILL_SEED, quote.fill_seed().as_ref()], bump,
    )]
    pub fill: Box<Account<'info, QuoteFill>>,
    /// CHECK: pinned to the Instructions sysvar; read only through the checked loaders.
    #[account(address = sysvar_instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// A mint at a payout: settled, or a $1 payout still in its delay. No quote.
#[event_cpi]
#[derive(Accounts)]
pub struct MintAtPayout<'info> {
    pub buyer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, market.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(mut, address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(
        mut, token::mint = usdc_mint, token::authority = buyer,
        constraint = buyer_usdc.key() != vault.key() @ ExchangeError::WrongAccount,
    )]
    pub buyer_usdc: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = ptoken_mint)]
    pub to: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    pub token_program: Program<'info, Token>,
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(amount_in: u64, min_out: u64, quote: Quote)]
pub struct RedeemQuoted<'info> {
    #[account(mut)]
    pub seller: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, market.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(mut, address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = ptoken_mint, token::authority = seller)]
    pub seller_ptoken: Box<Account<'info, TokenAccount>>,
    #[account(
        mut, address = market.held @ ExchangeError::WrongAccount,
        constraint = held.key() != seller_ptoken.key() @ ExchangeError::WrongAccount,
    )]
    pub held: Box<Account<'info, TokenAccount>>,
    /// Receives the USDC.
    #[account(mut, token::mint = usdc_mint, constraint = to.key() != vault.key() @ ExchangeError::WrongAccount)]
    pub to: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(
        init_if_needed, payer = seller, space = 8 + QuoteFill::INIT_SPACE,
        seeds = [FILL_SEED, quote.fill_seed().as_ref()], bump,
    )]
    pub fill: Box<Account<'info, QuoteFill>>,
    /// CHECK: pinned to the Instructions sysvar; read only through the checked loaders.
    #[account(address = sysvar_instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    /// CHECK: `["claim", claim_tail]`; created, with its seeds checked, only
    /// if part of the payment queues.
    #[account(mut)]
    pub new_claim: Option<UncheckedAccount<'info>>,
    /// CHECK: `["delayed", delayed_next]`; created, with its seeds checked,
    /// only if part of the sale passes the hourly cap.
    #[account(mut)]
    pub new_ticket: Option<UncheckedAccount<'info>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// A redemption of a settled market at its payout. No quote.
#[event_cpi]
#[derive(Accounts)]
pub struct RedeemAtPayout<'info> {
    #[account(mut)]
    pub seller: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, market.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(mut, address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = ptoken_mint, token::authority = seller)]
    pub seller_ptoken: Box<Account<'info, TokenAccount>>,
    #[account(
        mut, address = market.held @ ExchangeError::WrongAccount,
        constraint = held.key() != seller_ptoken.key() @ ExchangeError::WrongAccount,
    )]
    pub held: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = usdc_mint, constraint = to.key() != vault.key() @ ExchangeError::WrongAccount)]
    pub to: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    /// CHECK: as in `RedeemQuoted`.
    #[account(mut)]
    pub new_claim: Option<UncheckedAccount<'info>>,
    /// CHECK: as in `RedeemQuoted`.
    #[account(mut)]
    pub new_ticket: Option<UncheckedAccount<'info>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/* ---------------------------------------------------------------- float */

#[event_cpi]
#[derive(Accounts)]
pub struct Absorb<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, market.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(mut, address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(mut, address = market.held @ ExchangeError::WrongAccount)]
    pub held: Box<Account<'info, TokenAccount>>,
    /// Optional: another pToken account of this market the exchange PDA owns,
    /// normally its associated token account; all of it is burned.
    #[account(
        mut, token::mint = ptoken_mint, token::authority = config,
        constraint = other.key() != held.key() @ ExchangeError::WrongAccount,
    )]
    pub other: Option<Box<Account<'info, TokenAccount>>>,
    pub token_program: Program<'info, Token>,
}

/// `pay_queue`: anyone; `(claim, recipient, rent_to)` triples in `remaining_accounts`.
#[event_cpi]
#[derive(Accounts)]
pub struct PayQueue<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    pub token_program: Program<'info, Token>,
}

/// Pays a set-aside claim to the account it was owed to, never elsewhere
/// (that would route around a USDC freeze). Anyone may call.
#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawUnclaimed<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, constraint = claim.set_aside @ ExchangeError::NothingPending, close = rent_to)]
    pub claim: Box<Account<'info, Claim>>,
    /// CHECK: pinned to the claim's rent payer.
    #[account(mut, address = claim.payer @ ExchangeError::WrongAccount)]
    pub rent_to: UncheckedAccount<'info>,
    #[account(mut, address = claim.to @ ExchangeError::WrongAccount)]
    pub to: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    pub token_program: Program<'info, Token>,
}

/// Pays a delayed sale once `DELAY` has passed. Anyone may call.
#[event_cpi]
#[derive(Accounts)]
pub struct Release<'info> {
    /// Pays the rent of any claim the payment creates.
    #[account(mut)]
    pub caller: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, ticket.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(mut, address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(mut, address = market.held @ ExchangeError::WrongAccount)]
    pub held: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [DELAYED_SEED, &ticket.ticket.to_le_bytes()], bump = ticket.bump, close = rent_to)]
    pub ticket: Box<Account<'info, DelayedSale>>,
    /// CHECK: pinned to the ticket's rent payer.
    #[account(mut, address = ticket.payer @ ExchangeError::WrongAccount)]
    pub rent_to: UncheckedAccount<'info>,
    /// CHECK: pinned to the ticket's recipient; its state is checked before
    /// paying (it may have been closed or frozen since the sale).
    #[account(mut, address = ticket.to @ ExchangeError::WrongAccount)]
    pub to: UncheckedAccount<'info>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    /// CHECK: `["claim", claim_tail]`; created, with its seeds checked, only
    /// if part of the payment queues.
    #[account(mut)]
    pub new_claim: Option<UncheckedAccount<'info>>,
    /// CHECK: `["aside", aside_next]`; created, with its seeds checked, only
    /// if the recipient cannot be paid and the payment is set aside.
    #[account(mut)]
    pub new_aside: Option<UncheckedAccount<'info>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// Owner undoes a delayed sale: the pToken goes back to the sale's payee.
#[event_cpi]
#[derive(Accounts)]
pub struct Cancel<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = owner @ ExchangeError::OnlyOwner)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [MARKET_SEED, ticket.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
    #[account(mut, address = market.held @ ExchangeError::WrongAccount)]
    pub held: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [DELAYED_SEED, &ticket.ticket.to_le_bytes()], bump = ticket.bump, close = rent_to)]
    pub ticket: Box<Account<'info, DelayedSale>>,
    /// CHECK: pinned to the ticket's rent payer.
    #[account(mut, address = ticket.payer @ ExchangeError::WrongAccount)]
    pub rent_to: UncheckedAccount<'info>,
    #[account(
        mut, token::mint = ptoken_mint, token::authority = ticket.to_owner,
        constraint = dest.key() != held.key() @ ExchangeError::WrongAccount,
    )]
    pub dest: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct SendToBridge<'info> {
    pub keeper: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED], bump = config.bump,
        constraint = config.is_keeper(&keeper.key()) @ ExchangeError::OnlyKeeper,
        constraint = config.bridge_deposit != Pubkey::default() @ ExchangeError::NoBridge,
    )]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
    /// The destination is fixed by the owner.
    #[account(
        mut, address = config.bridge_deposit @ ExchangeError::WrongAccount, token::mint = usdc_mint,
        constraint = bridge.key() != vault.key() @ ExchangeError::WrongAccount,
    )]
    pub bridge: Box<Account<'info, TokenAccount>>,
    #[account(address = config.usdc_mint @ ExchangeError::WrongAccount)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    pub token_program: Program<'info, Token>,
}

/// Returns tokens sent to the exchange PDA by mistake. Never USDC, never a
/// pToken (any mint the exchange PDA can mint).
#[event_cpi]
#[derive(Accounts)]
pub struct Rescue<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = owner @ ExchangeError::OnlyOwner)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        mut, token::authority = config,
        constraint = source.mint != config.usdc_mint && source.key() != config.vault @ ExchangeError::NotRescuable,
    )]
    pub source: Box<Account<'info, TokenAccount>>,
    #[account(
        address = source.mint @ ExchangeError::WrongAccount,
        constraint = mint.mint_authority != COption::Some(config.key()) @ ExchangeError::NotRescuable,
    )]
    pub mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = mint, constraint = dest.key() != source.key() @ ExchangeError::WrongAccount)]
    pub dest: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

/* ---------------------------------------------------------------- views */

/// `view_float`. Read-only, no signer: callable in a simulation.
#[derive(Accounts)]
pub struct ViewFloat<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(address = config.vault @ ExchangeError::WrongAccount)]
    pub vault: Box<Account<'info, TokenAccount>>,
}

/// `view_market`. Read-only, no signer.
#[derive(Accounts)]
pub struct ViewMarket<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [MARKET_SEED, market.position_id.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(address = market.mint @ ExchangeError::WrongAccount)]
    pub ptoken_mint: Box<Account<'info, Mint>>,
}

/// `preview_mint` and `preview_redeem`. Read-only, no signer.
#[derive(Accounts)]
pub struct ViewConfig<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
}

/// Closes an expired quote's fill record, its rent back to whoever paid it.
#[derive(Accounts)]
pub struct CloseFill<'info> {
    #[account(mut, close = payer)]
    pub fill: Box<Account<'info, QuoteFill>>,
    /// CHECK: pinned to the fill's rent payer.
    #[account(mut, address = fill.payer @ ExchangeError::WrongAccount)]
    pub payer: UncheckedAccount<'info>,
}
