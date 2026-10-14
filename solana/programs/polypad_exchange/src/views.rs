//! Read-only views: the EVM's `freeFloat`, `outflowRemaining`, `queueLength`,
//! `delayedLength`, `maxRisk`, `mintOut`, `redeemOut`, `oracle.status` and
//! `oracle.settlement`, computed by the program's own math so clients and the
//! launch program never re-implement its rounding. Each view instruction
//! returns its value as return data; clients call them in a simulated
//! transaction (Anchor's `.view()` in TypeScript), so they cost nothing and
//! change nothing.

use anchor_lang::prelude::*;

use crate::math::{mint_out, redeem_out};
use crate::state::*;
use crate::trade::payout_mint;

/// The float and the queue, as the next instruction would see them.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct FloatView {
    /// USDC in the float vault.
    pub vault: u64,
    /// What can pay a redemption now or go to the bridge (`freeFloat`).
    pub free_float: u64,
    pub queued: u64,
    pub unclaimed_total: u64,
    /// Next claim in line and the next index a claim would take: the queue
    /// is `claim_head..claim_tail` (`queueLength` is their difference).
    pub claim_head: u64,
    pub claim_tail: u64,
    /// Next set-aside claim index and next delayed-sale ticket (`delayedLength`).
    pub aside_next: u64,
    pub delayed_next: u64,
    /// USDC metered redemptions may still pay this sliding hour (`outflowRemaining`).
    pub outflow_remaining: u64,
    pub total_risk: u128,
    pub max_total_risk: u64,
    pub halted: bool,
}

/// One market, as the next instruction would see it: burns made straight
/// through the token program are already counted.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct MarketView {
    pub supply: u64,
    pub backed: u64,
    pub minted_since_report: u64,
    pub minted_total: u64,
    /// Every share burned so far, direct burns included: the `seen` a keeper
    /// passes to `report_backed` with the holdings it reads now.
    pub burned_total: u64,
    pub delayed_shares: u64,
    /// Stored risk (12 decimals) and the market's cap in USDC (`maxRisk`).
    pub risk: u128,
    pub max_risk: u64,
    pub paused: bool,
    /// A payout is in effect: redeem and mint at it with no quote.
    pub settled: bool,
    /// A mint prices at the payout (settled, or a pending $1 payout).
    pub mints_at_payout: bool,
    pub payout: u64,
    /// When the payout takes effect; 0 = none recorded.
    pub settle_at: i64,
}

pub fn float_view(c: &Config, vault: u64, now: i64) -> FloatView {
    FloatView {
        vault,
        free_float: c.free_float(vault),
        queued: c.queued,
        unclaimed_total: c.unclaimed_total,
        claim_head: c.claim_head,
        claim_tail: c.claim_tail,
        aside_next: c.aside_next,
        delayed_next: c.delayed_next,
        outflow_remaining: c.outflow_remaining(now, vault),
        total_risk: c.total_risk,
        max_total_risk: c.max_total_risk,
        halted: c.halted,
    }
}

/// `c` and `m` are copies: the view counts direct burns on them without
/// writing anything back.
pub fn market_view(c: &mut Config, m: &mut Market, supply: u64, now: i64) -> MarketView {
    c.burned_outside(m, supply);
    MarketView {
        supply,
        backed: m.backed,
        minted_since_report: m.minted_since_report,
        minted_total: m.minted_total,
        burned_total: m.burned_total,
        delayed_shares: m.delayed_shares,
        risk: m.risk,
        max_risk: c.max_risk(m),
        paused: m.paused,
        settled: m.settled(now),
        mints_at_payout: payout_mint(m, now).is_some(),
        payout: m.payout,
        settle_at: m.settle_at,
    }
}

/// pToken a mint of `usdc_in` at `price` pays out with the buy spread (`mintOut`).
pub fn preview_mint(c: &Config, price: u64, usdc_in: u64) -> Result<u64> {
    mint_out(price, usdc_in, c.buy_spread_bps)
}

/// USDC a redemption of `amount_in` at `price` pays: less the sell spread,
/// or less the settle fee at a payout (`redeemOut`).
pub fn preview_redeem(c: &Config, price: u64, amount_in: u64, settled: bool) -> Result<u64> {
    redeem_out(price, amount_in, if settled { c.settle_fee_bps } else { c.sell_spread_bps })
}
