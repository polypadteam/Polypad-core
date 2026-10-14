//! Events, mirroring `LaunchFactory`, `FeeVault` and the Graduator's fee
//! role. Emitted with `emit_cpi!` (a self-CPI), so indexers read them from
//! instruction data instead of logs that can be truncated.

use anchor_lang::prelude::*;

use crate::state::Fees;

#[event]
pub struct OwnershipTransferStarted {
    pub owner: Pubkey,
    pub pending_owner: Pubkey,
}

#[event]
pub struct OwnershipTransferred {
    pub previous: Pubkey,
    pub owner: Pubkey,
}

#[event]
pub struct ConfigSet {
    pub platform: Pubkey,
    pub grad_usd: u64,
}

#[event]
pub struct FeesSet {
    pub fees: Fees,
}

#[event]
pub struct RolesSet {
    pub keeper: Pubkey,
    pub holder_payout: Pubkey,
}

#[event]
pub struct Launched {
    pub position_id: [u8; 32],
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub pool: Pubkey,
    pub dbc_config: Pubkey,
    pub ptoken_mint: Pubkey,
    pub price: u64,
    pub phantom: u64,
    pub holders_bps: u16,
}

#[event]
pub struct Graduated {
    pub base_mint: Pubkey,
    pub damm_pool: Pubkey,
    pub position: Pubkey,
    pub locked_liquidity: u128,
    /// Whether this `graduate` ran DBC's migration itself, or found it done.
    pub migrated_here: bool,
}

/// A launch whose pool was not opened before its quote expired: the record
/// is closed, and any pool opened on its config is only a look-alike.
#[event]
pub struct LaunchExpired {
    pub base_mint: Pubkey,
    pub pool: Pubkey,
    pub opened: bool,
}

/// One `collect`: pToken claimed from the curve and from the pool, and how
/// the total was split.
#[event]
pub struct FeesCollected {
    pub base_mint: Pubkey,
    pub curve: u64,
    pub pool: u64,
    pub payee: Pubkey,
    pub to_payee: u64,
    pub to_holders: u64,
    pub to_platform: u64,
}

#[event]
pub struct PayeeProposed {
    pub base_mint: Pubkey,
    pub payee: Pubkey,
    pub pending_payee: Pubkey,
}

#[event]
pub struct PayeeSet {
    pub base_mint: Pubkey,
    pub previous: Pubkey,
    pub payee: Pubkey,
}

#[event]
pub struct Withdrawn {
    pub mint: Pubkey,
    pub payee: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct HoldersPaidOut {
    pub base_mint: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct LeftoverBurned {
    pub base_mint: Pubkey,
    pub amount: u64,
}

/// Partner fees claimed from a DBC pool that is not a Polypad coin (one
/// opened on our config or a look-alike's), credited to the platform.
#[event]
pub struct FeesSwept {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base: u64,
    pub quote: u64,
    pub platform: Pubkey,
}

/// Fees claimed from a DAMM v2 position our vault holds in a pool that is
/// not a Polypad coin's (a late coin's or a look-alike's, migrated by
/// anyone), credited to the platform.
#[event]
pub struct PositionFeesSwept {
    pub pool: Pubkey,
    pub position: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub fee_a: u64,
    pub fee_b: u64,
    pub platform: Pubkey,
}
