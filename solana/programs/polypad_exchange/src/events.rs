//! Events, mirroring `PriceOracle` and `PExchange`'s. Emitted through a
//! self-CPI (`emit_cpi!`'s mechanism), so indexers read them from the
//! instruction data instead of logs that can be truncated.

use anchor_lang::prelude::*;

#[event]
pub struct Initialized {
    pub owner: Pubkey,
    pub usdc_mint: Pubkey,
    pub vault: Pubkey,
}

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
pub struct SignerSet {
    pub signer: Pubkey,
}

#[event]
pub struct SignerProposed {
    pub signer: Pubkey,
    pub effective_at: i64,
}

#[event]
pub struct PausedSet {
    pub position_id: [u8; 32],
    pub paused: bool,
}

#[event]
pub struct Settled {
    pub position_id: [u8; 32],
    pub payout: u64,
    pub effective_at: i64,
}

#[event]
pub struct SettleCancelled {
    pub position_id: [u8; 32],
}

#[event]
pub struct PTokenCreated {
    pub position_id: [u8; 32],
    pub mint: Pubkey,
}

#[event]
pub struct Minted {
    pub position_id: [u8; 32],
    pub to: Pubkey,
    pub usdc_in: u64,
    pub p_out: u64,
    pub price: u64,
}

#[event]
pub struct Redeemed {
    pub position_id: [u8; 32],
    pub to: Pubkey,
    pub p_in: u64,
    pub usdc_out: u64,
    pub price: u64,
}

#[event]
pub struct Absorbed {
    pub position_id: [u8; 32],
    pub amount: u64,
}

#[event]
pub struct BackedReported {
    pub position_id: [u8; 32],
    pub amount: u64,
}

#[event]
pub struct SentToBridge {
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct ParamsSet {
    pub buy_spread_bps: u16,
    pub sell_spread_bps: u16,
    pub min_price: u64,
    pub max_price: u64,
    pub default_max_risk: u64,
}

#[event]
pub struct RolesSet {
    pub keeper: Pubkey,
    pub factory: Pubkey,
    pub bridge_deposit: Pubkey,
}

#[event]
pub struct BridgeDepositProposed {
    pub bridge_deposit: Pubkey,
    pub effective_at: i64,
}

#[event]
pub struct Rescued {
    pub mint: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct OutflowCapSet {
    pub floor: u64,
    pub float_bps: u64,
}

#[event]
pub struct MeterSalesSet {
    pub on: bool,
}

#[event]
pub struct SaleDelayed {
    pub ticket: u64,
    pub position_id: [u8; 32],
    pub to: Pubkey,
    pub p_amount: u64,
    pub amount: u64,
    pub ready_at: i64,
}

#[event]
pub struct Released {
    pub ticket: u64,
    pub position_id: [u8; 32],
}

#[event]
pub struct Cancelled {
    pub ticket: u64,
}

#[event]
pub struct HaltSet {
    pub halted: bool,
}

#[event]
pub struct Queued {
    pub ticket: u64,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct ClaimPaid {
    pub ticket: u64,
    pub to: Pubkey,
    pub amount: u64,
}

/// A queued claim whose recipient could not be paid: set aside for its owner.
#[event]
pub struct ClaimUndelivered {
    pub ticket: u64,
    pub to: Pubkey,
    pub amount: u64,
}

/// A release that could not pay its recipient: set aside at `claim`
/// (`["aside", index]`). The EVM adds it to `unclaimed[to]` silently.
#[event]
pub struct SetAside {
    pub claim: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct UnclaimedWithdrawn {
    pub claim: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct SettleFeeSet {
    pub settle_fee_bps: u16,
}

#[event]
pub struct MaxTotalRiskSet {
    pub max_total_risk: u64,
}

#[event]
pub struct MaxRiskSet {
    pub position_id: [u8; 32],
    pub cap: u64,
}

#[event]
pub struct ShareLabelSet {
    pub position_id: [u8; 32],
    pub name: String,
    pub symbol: String,
}

/// `emit_cpi!` as a function, so the shared payment paths can emit without a
/// `ctx` in scope. `authority` is the `#[event_cpi]` account the caller's
/// accounts struct already constrained to the event authority PDA.
pub fn emit<E: anchor_lang::Event>(authority: &AccountInfo, event: &E) -> Result<()> {
    let data: Vec<u8> = anchor_lang::event::EVENT_IX_TAG_LE.iter().copied().chain(event.data()).collect();
    let ix = anchor_lang::solana_program::instruction::Instruction::new_with_bytes(
        crate::ID,
        &data,
        vec![anchor_lang::solana_program::instruction::AccountMeta::new_readonly(*authority.key, true)],
    );
    anchor_lang::solana_program::program::invoke_signed(
        &ix,
        std::slice::from_ref(authority),
        &[&[b"__event_authority", &[crate::EVENT_AUTHORITY_AND_BUMP.1]]],
    )
    .map_err(Into::into)
}
