//! `PExchange`'s pure math and its risk and outflow bookkeeping, on plain
//! structs so it is tested directly (below) as well as through the program.
//! Every intermediate is u128 and checked; every rounding favours the float,
//! exactly as the Solidity rounds.

use anchor_lang::prelude::*;

use crate::error::ExchangeError;
use crate::state::{Config, HourFloat, Market, BPS, HOUR, ONE};

fn overflow() -> Error {
    ExchangeError::MathOverflow.into()
}

pub fn mul_div_up(a: u128, b: u128, d: u128) -> Result<u128> {
    let p = a.checked_mul(b).ok_or_else(overflow)?;
    Ok(p.checked_add(d - 1).ok_or_else(overflow)? / d)
}

fn to_u64(v: u128) -> Result<u64> {
    u64::try_from(v).map_err(|_| overflow())
}

/// pToken minted for `usdc_in` at `price`: the price with the spread is
/// rounded up, then the shares down (`_mintOut`).
pub fn mint_out(price: u64, usdc_in: u64, spread_bps: u16) -> Result<u64> {
    let eff = mul_div_up(price as u128, BPS as u128 + spread_bps as u128, BPS as u128)?;
    if eff == 0 {
        return Err(overflow());
    }
    to_u64((usdc_in as u128).checked_mul(ONE as u128).ok_or_else(overflow)? / eff)
}

/// USDC paid for `amount_in` pToken at `price`: the price less the spread is
/// rounded down, then the USDC down (`_redeemOut`).
pub fn redeem_out(price: u64, amount_in: u64, spread_bps: u16) -> Result<u64> {
    let eff = (price as u128) * (BPS as u128 - spread_bps as u128) / BPS as u128;
    to_u64((amount_in as u128).checked_mul(eff).ok_or_else(overflow)? / ONE as u128)
}

/// `risk`, no more than the unbacked shares at `low` could carry (`_bounded`).
pub fn bounded(risk: u128, supply: u64, held: u64, low: u64) -> u128 {
    if supply <= held {
        return 0;
    }
    let bound = (supply - held) as u128 * (ONE - low.min(ONE)) as u128;
    risk.min(bound)
}

/// A deposit counts toward the cap only once it has sat through an hour's turn (`_base`).
pub fn base(f: &HourFloat, cur: u64) -> u64 {
    if f.hour == 0 || cur < f.amount {
        cur
    } else {
        f.amount
    }
}

pub fn hour_of(now: i64) -> u64 {
    (now.max(0) / HOUR) as u64
}

impl Config {
    /// Float not owed to the queue: what can pay a redemption now or go to the bridge.
    pub fn free_float(&self, vault_amount: u64) -> u64 {
        vault_amount.saturating_sub(self.queued)
    }

    pub fn max_risk(&self, m: &Market) -> u64 {
        if m.max_risk_override == 0 {
            self.default_max_risk
        } else {
            m.max_risk_override
        }
    }

    /// Record the float for the outflow cap at the hour's first trade, before it moves it (`_markHour`).
    pub fn mark_hour(&mut self, now: i64, vault_amount: u64) {
        let hour = hour_of(now);
        let f = self.hour_float;
        if f.hour == hour {
            return;
        }
        let cur = self.free_float(vault_amount);
        self.hour_float = HourFloat { hour, amount: cur, base: base(&f, cur) };
    }

    /// Metered USDC in the sliding hour: the previous clock hour counts for
    /// the part of it still inside the last 3,600 seconds (`_sliding`).
    pub fn outflow_used(&self, now: i64) -> u64 {
        let hour = hour_of(now);
        let into = (now.max(0) % HOUR) as u128;
        let prev = if hour == 0 { 0 } else { self.outflow.at(hour - 1) };
        let carried = (prev as u128 * (HOUR as u128 - into) / HOUR as u128) as u64;
        self.outflow.at(hour).saturating_add(carried)
    }

    /// USDC that may still leave through metered redemptions this hour (`outflowRemaining`).
    pub fn outflow_remaining(&self, now: i64, vault_amount: u64) -> u64 {
        let hour = hour_of(now);
        let used = self.outflow_used(now);
        let f = self.hour_float;
        let b = if f.hour == hour { f.base } else { base(&f, self.free_float(vault_amount)) };
        let mut cap = (b as u128 * self.outflow_float_bps as u128 / BPS as u128) as u64;
        if cap < self.outflow_floor {
            cap = self.outflow_floor;
        }
        cap.saturating_sub(used)
    }

    /// Store a market's risk and keep `total_risk` the sum of them (`_storeRisk`).
    pub fn store_risk(&mut self, m: &mut Market, risk: u128, low: u64, high: u64) -> Result<()> {
        self.total_risk = self.total_risk.checked_add(risk).ok_or_else(overflow)?.checked_sub(m.risk).ok_or_else(overflow)?;
        m.risk = risk;
        m.low = low;
        m.high = high;
        Ok(())
    }

    /// The unbacked risk cap on a quoted mint (`_checkRisk`). `supply` is the
    /// pre-mint supply, shares held for delayed sales included (a cancel
    /// hands them back). The risk carried is what the unbacked shares' mints
    /// added, so minting at rising prices cannot re-value cheap ones; it never
    /// exceeds every unbacked share at the lowest price minted since the
    /// market was last fully backed.
    pub fn check_risk(&mut self, m: &mut Market, supply: u64, out: u64, price: u64) -> Result<()> {
        let held = m.backed;
        let (mut risk, mut low, mut high) = (m.risk, m.low, m.high);
        if supply <= held {
            (risk, low, high) = (0, price, price);
        } else {
            // Unknown (backing lost since): every unbacked share at $1. No real
            // price is $1, so only a lowered report sets this.
            if high == ONE {
                risk = (supply - held) as u128 * ONE as u128;
            }
            low = low.min(price);
            high = high.max(price);
            risk = bounded(risk, supply, held, low);
        }
        risk = risk.checked_add(out as u128 * (ONE - price) as u128).ok_or_else(overflow)?;
        if risk / ONE as u128 > self.max_risk(m) as u128 {
            return err!(ExchangeError::RiskCap);
        }
        self.store_risk(m, risk, low, high)?;
        m.minted_since_report = m.minted_since_report.checked_add(out).ok_or_else(overflow)?;
        if self.total_risk / ONE as u128 > self.max_total_risk as u128 {
            return err!(ExchangeError::RiskCap);
        }
        Ok(())
    }

    /// Count shares holders burned straight through the token program since
    /// the exchange last looked. On the EVM only the exchange could burn a
    /// PToken; an SPL Token holder can burn their own, which lowers the supply
    /// and nothing else. `supply` is the supply before this instruction's own
    /// mint or burn. Those shares are booked as an exchange burn books them:
    /// up to `minted_since_report` of them leave the unbacked gap, taking
    /// (1 - high) of risk each off as they go, and the rest lower backing,
    /// which never stays above the supply. The caller's bound at the lower
    /// supply (`check_risk`, `burned`, `refresh`) then frees the room those
    /// shares used, as an exchange redemption would, and no more: shares that
    /// no longer exist carry no exposure. The supply is noted for the next
    /// report (`low_supply`). A direct burn never makes an exchange
    /// instruction fail.
    pub fn burned_outside(&mut self, m: &mut Market, supply: u64) {
        // Between instructions the supply only falls (only the exchange
        // mints), so what this one sees is the lowest since the last.
        m.low_supply = m.low_supply.min(supply);
        let gone = m.minted_total.saturating_sub(supply).saturating_sub(m.burned_total);
        if gone == 0 {
            // Backing is already within the supply after any instruction;
            // this keeps it so whatever happened before.
            m.backed = m.backed.min(supply);
            return;
        }
        m.burned_total = m.burned_total.saturating_add(gone);
        let gap = supply.saturating_add(gone).saturating_sub(m.backed);
        let k = m.minted_since_report.min(gone).min(gap);
        m.minted_since_report -= k;
        m.backed = m.backed.saturating_sub(gone - k).min(supply);
        // As `burned` takes off for the same shares redeemed. It only lowers
        // a market's risk and the total by the same, so it cannot fail.
        let off = (k as u128 * (ONE - m.high.min(ONE)) as u128).min(m.risk);
        m.risk -= off;
        self.total_risk = self.total_risk.saturating_sub(off);
    }

    /// `n` shares left the supply (already burned; `supply` is after the burn).
    /// Up to `minted_since_report` of them are taken as shares the desk may not
    /// have bought yet: they leave the unbacked gap with at least (1 - high)
    /// of risk each. The rest were backed: backing drops with them, so the
    /// desk's shares for them, which it will sell, cannot back a new mint (`_burned`).
    pub fn burned(&mut self, m: &mut Market, n: u64, supply: u64, now: i64) -> Result<()> {
        // Burns the exchange never saw come first, against the supply before this one.
        self.burned_outside(m, supply.checked_add(n).ok_or_else(overflow)?);
        m.burned_total = m.burned_total.checked_add(n).ok_or_else(overflow)?;
        let mut b = m.backed;
        // The pre-burn unbacked gap. `burned_outside` keeps backing within
        // the supply, so this cannot go below zero; it saturates anyway, as a
        // burn must never be refused over the books.
        let gap = supply.saturating_add(n).saturating_sub(b);
        let k = m.minted_since_report.min(n).min(gap);
        m.minted_since_report -= k;
        let backed_part = n - k;
        b = b.saturating_sub(backed_part).min(supply);
        m.backed = b;
        m.low_supply = m.low_supply.min(supply);
        if m.risk == 0 {
            return Ok(());
        }
        if m.settled(now) {
            return self.store_risk(m, 0, 0, 0);
        }
        let off = k as u128 * (ONE - m.high.min(ONE)) as u128;
        let (risk, low, high) = (m.risk, m.low, m.high);
        self.store_risk(m, bounded(risk.saturating_sub(off), supply, b, low), low, high)
    }

    /// One market's backing report (`_report`): shares the desk holds, as it
    /// read them when `burned_total` was `seen` and `minted_total` was
    /// `minted`. Returns false (skipped) for a read against another state.
    /// Skipped or not, it first books the direct burns since the last visit,
    /// into the market and `total_risk` alike, so the caller must store the
    /// market whatever this returns (`report_backed` does, through
    /// `with_market`), or the total drifts from the sum of the markets.
    ///
    /// `seen` is only a consistency check now (a read ahead of the market's
    /// counts is skipped); the bound uses `minted` and the market's own
    /// counts, never `seen`.
    ///
    /// The desk sells only shares beyond the supply, so after its read it
    /// holds at least what it read, or the lowest supply since, whichever is
    /// less. The report takes the lowest it can prove that supply was, and
    /// never lowers backing for burns alone: what was backed is still held
    /// (the same rule, since the last report). So a share minted after the
    /// read and burned before the report changes nothing here: its mint added
    /// its risk and its burn took it off, as for any share minted and burned
    /// between two reports. Only the desk holding less than was backed, as it
    /// read it, is lost backing. A deliberate divergence from PExchange v14
    /// `_report` (EVM-PORT-SPEC 3).
    pub fn report(&mut self, m: &mut Market, supply: u64, amount: u64, seen: u64, minted: u64, now: i64) -> Result<bool> {
        // Direct burns count as burns here too, and show in `low_supply`.
        self.burned_outside(m, supply);
        // A desk read against another state (a keeper bug) is skipped, not
        // allowed to stop the rest of the batch: counts ahead of the market's,
        // or more burned than minted.
        if seen > m.burned_total || minted > m.minted_total || seen > minted {
            return Ok(false);
        }
        let was = m.backed;
        // Backing lost is what the desk held below what was backed, as it read
        // it: shares burned since were either taken off backing already or
        // were never held, so they are not lost again.
        let lost = was.saturating_sub(amount);
        // The lowest the supply can have been since the read: every share
        // burned since then burned before any minted since (`minted` less all
        // burns now); or, for a read after the last report, the lowest seen
        // since it, and for one before, as low as the burns up to the report
        // could take it.
        let least = minted
            .saturating_sub(m.burned_total)
            .max(m.low_supply.min(minted.saturating_sub(m.burned_at_report)));
        // Nothing lost: the desk still holds what was backed (it held it at the
        // last report and has sold only down to the supply since). The final
        // clamp is redundant defence: `least` is at most `minted_total -
        // burned_total` (the supply) or `low_supply` (just lowered to the
        // supply), and `was` is within the supply after `burned_outside`.
        let amount = if lost == 0 { amount.min(least.max(was)) } else { amount.min(least) }.min(supply);
        let (mut risk, mut low, mut high) = (m.risk, m.low, m.high);
        m.backed = amount;
        m.minted_since_report = 0;
        m.low_supply = supply;
        m.burned_at_report = m.burned_total;
        if m.settled(now) {
            risk = 0;
        } else if lost > 0 {
            // Marked unknown: the next mint counts the whole gap at $1. A paused
            // market takes no mints (the desk may be unwinding a decided one),
            // so its lost backing is left out of the total until then; the risk
            // it already carried (forged fills included) stays.
            if !m.paused {
                risk = risk.checked_add(lost as u128 * ONE as u128).ok_or_else(overflow)?;
            }
            (low, high) = (0, ONE);
        } else if amount > was {
            // Hedged shares carried at least (1 - high) each, whichever they were.
            let off = (amount - was) as u128 * (ONE - high.min(ONE)) as u128;
            risk = bounded(risk.saturating_sub(off), supply, amount, low);
        }
        // Otherwise nothing lost and backing as it was (it never falls here):
        // the risk stands.
        self.store_risk(m, risk, low, high)?;
        Ok(true)
    }

    /// Bring a market's stored risk down to what its unbacked supply can carry
    /// now, or to 0 once settled (`refreshRisk`). Only ever lowers it.
    pub fn refresh(&mut self, m: &mut Market, supply: u64, now: i64) -> Result<()> {
        self.burned_outside(m, supply);
        if m.risk == 0 {
            return Ok(());
        }
        if m.settled(now) {
            return self.store_risk(m, 0, 0, 0);
        }
        let (risk, low, high) = (m.risk, m.low, m.high);
        let b = m.backed;
        self.store_risk(m, bounded(risk, supply, b, low), low, high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            owner: Pubkey::default(),
            pending_owner: Pubkey::default(),
            keeper: Pubkey::default(),
            factory: Pubkey::default(),
            signer: Pubkey::default(),
            pending_signer: Pubkey::default(),
            pending_signer_at: 0,
            usdc_mint: Pubkey::default(),
            vault: Pubkey::default(),
            bridge_deposit: Pubkey::default(),
            pending_bridge_deposit: Pubkey::default(),
            pending_bridge_deposit_at: 0,
            bridge_ever_set: false,
            halted: false,
            buy_spread_bps: 25,
            sell_spread_bps: 25,
            settle_fee_bps: 50,
            min_price: 1_000,
            max_price: 999_000,
            meter_sales: true,
            default_max_risk: 5_000 * ONE,
            max_total_risk: 25_000 * ONE,
            total_risk: 0,
            outflow_floor: 5_000 * ONE,
            outflow_float_bps: 5_000,
            hour_float: HourFloat::default(),
            outflow: Default::default(),
            queued: 0,
            unclaimed_total: 0,
            claim_head: 0,
            claim_tail: 0,
            aside_next: 0,
            delayed_next: 0,
            bump: 0,
            vault_bump: 0,
        }
    }

    fn market() -> Market {
        Market {
            position_id: [1; 32],
            mint: Pubkey::default(),
            held: Pubkey::default(),
            paused: false,
            paused_before_settle: false,
            payout: 0,
            settle_at: 0,
            settled_by: Pubkey::default(),
            barred: Pubkey::default(),
            backed: 0,
            max_risk_override: 0,
            risk: 0,
            low: 0,
            high: 0,
            minted_since_report: 0,
            minted_total: 0,
            burned_total: 0,
            low_supply: 0,
            burned_at_report: 0,
            delayed_shares: 0,
            name: String::new(),
            symbol: String::new(),
            bump: 0,
            mint_bump: 0,
            held_bump: 0,
        }
    }

    #[test]
    fn mint_and_redeem_round_like_the_solidity() {
        // 600 USDC at 60c + 0.25%: the price rounds up to 601_500, shares down.
        let eff = (600_000u128 * 10_025).div_ceil(10_000);
        assert_eq!(mint_out(600_000, 600 * ONE, 25).unwrap() as u128, 600 * ONE as u128 * ONE as u128 / eff);
        // 1 share at 59c - 0.25%: price rounds down, USDC down.
        assert_eq!(redeem_out(590_000, ONE, 25).unwrap(), 590_000 * 9_975 / 10_000);
        // An odd price where the up-rounding matters.
        assert_eq!(mint_out(333_333, 1_000_000, 25).unwrap(), 1_000_000 * 1_000_000 / 334_167);
        assert_eq!(redeem_out(0, ONE, 50).unwrap(), 0);
    }

    #[test]
    fn mint_then_redeem_never_profits() {
        for price in [1_000u64, 7_777, 333_333, 600_000, 999_000] {
            for usdc in [1u64, 999, 1_000_001, 123_456_789, 10_000 * ONE] {
                for spread in [0u16, 1, 25, 500] {
                    let out = mint_out(price, usdc, spread).unwrap();
                    // Selling back at the same price with no spread returns no more than paid.
                    assert!(redeem_out(price, out, 0).unwrap() <= usdc, "{price} {usdc} {spread}");
                }
            }
        }
    }

    #[test]
    fn mint_out_overflow_is_an_error_not_a_wrap() {
        assert!(mint_out(1, u64::MAX, 0).is_err());
    }

    #[test]
    fn bounded_caps_at_the_gap_times_one_minus_low() {
        assert_eq!(bounded(500, 10, 10, 0), 0);
        assert_eq!(bounded(u128::MAX, 10, 4, 600_000), 6 * 400_000);
        assert_eq!(bounded(7, 10, 4, 600_000), 7);
    }

    #[test]
    fn sliding_hour_counts_the_previous_hour_for_what_is_left_of_it() {
        let mut c = config();
        let h = 500_000u64;
        c.outflow.add(h, 1_000).unwrap();
        let start = (h as i64 + 1) * HOUR;
        assert_eq!(c.outflow_used(start), 1_000);
        assert_eq!(c.outflow_used(start + HOUR / 2), 500);
        assert_eq!(c.outflow_used(start + HOUR), 0);
        c.outflow.add(h + 1, 10).unwrap();
        assert_eq!(c.outflow_used(start + HOUR / 4), 10 + 750);
        // Skipping an hour drops the old slot.
        c.outflow.add(h + 5, 1).unwrap();
        assert_eq!(c.outflow.at(h + 1), 0);
        assert_eq!(c.outflow.at(h + 4), 0);
        // A clock that steps back adds to the current slot.
        c.outflow.add(h + 4, 2).unwrap();
        assert_eq!(c.outflow.at(h + 5), 3);
    }

    #[test]
    fn cap_base_is_the_hours_opening_float_and_never_rises_inside_it() {
        let mut c = config();
        c.outflow_floor = 0;
        let t = 1_000 * HOUR;
        c.mark_hour(t, 100 * ONE);
        assert_eq!(c.outflow_remaining(t, 100 * ONE), 50 * ONE);
        // Money arriving mid-hour does not lift this hour's cap...
        assert_eq!(c.outflow_remaining(t + 10, 1_000 * ONE), 50 * ONE);
        // ...nor the next hour's, which takes the lower of the two snapshots.
        c.mark_hour(t + HOUR, 1_000 * ONE);
        assert_eq!(c.outflow_remaining(t + HOUR, 1_000 * ONE), 50 * ONE);
        // The floor holds on a small float.
        c.outflow_floor = 80 * ONE;
        assert_eq!(c.outflow_remaining(t + HOUR, 1_000 * ONE), 80 * ONE);
    }

    /// A quoted mint of `out` on `supply`, booked as `trade::buy` books it.
    fn buy(c: &mut Config, m: &mut Market, supply: u64, out: u64, price: u64) {
        c.check_risk(m, supply, out, price).unwrap();
        m.minted_total += out;
    }

    #[test]
    fn risk_cap_counts_dollars_at_risk_and_total_is_the_sum() {
        let mut c = config();
        let mut a = market();
        let mut b = market();
        c.default_max_risk = 200 * ONE;
        // 482 shares at 60c risk $192.8.
        buy(&mut c, &mut a, 0, 482 * ONE, 600_000);
        assert_eq!(a.risk, 482 * ONE as u128 * 400_000);
        // Another 166 shares would pass $200.
        let before = (a.risk, c.total_risk);
        assert!(c.clone().check_risk(&mut a.clone(), 482 * ONE, 166 * ONE, 600_000).is_err());
        assert_eq!((a.risk, c.total_risk), before);
        c.check_risk(&mut b, 0, 100 * ONE, 970_000).unwrap();
        assert_eq!(c.total_risk, a.risk + b.risk);
        // Hedging all of a frees its room.
        c.report(&mut a, 482 * ONE, 482 * ONE, 0, 482 * ONE, 0).unwrap();
        assert_eq!(a.risk, 0);
        assert_eq!(c.total_risk, b.risk);
        c.check_risk(&mut a, 482 * ONE, 166 * ONE, 600_000).unwrap();
    }

    #[test]
    fn total_cap_refuses_across_markets() {
        let mut c = config();
        c.max_total_risk = 300 * ONE;
        let mut a = market();
        let mut b = market();
        c.check_risk(&mut a, 0, 400 * ONE, 500_000).unwrap(); // $200
        assert!(c.clone().check_risk(&mut b.clone(), 0, 400 * ONE, 500_000).is_err()); // $400 total
        c.check_risk(&mut b, 0, 200 * ONE, 500_000).unwrap(); // $300 total
    }

    #[test]
    fn lost_backing_marks_unknown_unless_paused() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100 * ONE, 900_000);
        c.report(&mut m, 100 * ONE, 100 * ONE, 0, 100 * ONE, 0).unwrap();
        assert_eq!(m.risk, 0);
        // The desk now reads 60: 40 lost, counted at $1 each.
        c.report(&mut m, 100 * ONE, 60 * ONE, 0, 100 * ONE, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.low, m.high), (60 * ONE, 40 * ONE as u128 * ONE as u128, 0, ONE));
        assert_eq!(c.total_risk, m.risk);
        // On a paused market the carried risk stays and nothing is added.
        let mut p = market();
        p.backed = 50 * ONE;
        p.minted_total = 50 * ONE;
        p.paused = true;
        c.report(&mut p, 50 * ONE, 10 * ONE, 0, 50 * ONE, 0).unwrap();
        assert_eq!((p.backed, p.risk, p.high), (10 * ONE, 0, ONE));
    }

    /// Phase 4 review: once backing was lost, the next mint counts the whole
    /// unbacked gap at $1, not the (1 - price) its shares were minted at.
    #[test]
    fn a_mint_after_lost_backing_counts_the_whole_gap_at_a_dollar() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100 * ONE, 600_000);
        // 80 hedged: a 20-share gap at 60c carries $8.
        c.report(&mut m, 100 * ONE, 80 * ONE, 0, 100 * ONE, 0).unwrap();
        assert_eq!(m.risk, 8 * ONE as u128 * ONE as u128);
        // 10 lost: $1 each on top, $18, and the market is marked unknown.
        c.report(&mut m, 100 * ONE, 70 * ONE, 0, 100 * ONE, 0).unwrap();
        assert_eq!((m.risk, m.high), (18 * ONE as u128 * ONE as u128, ONE));
        // One more share: the 30-share gap at $1, plus the new share at 40c.
        c.check_risk(&mut m, 100 * ONE, ONE, 600_000).unwrap();
        assert_eq!(m.risk, 30 * ONE as u128 * ONE as u128 + ONE as u128 * 400_000);
        assert_eq!(c.total_risk, m.risk);
        // Still unknown until fully backed: the next mint counts the gap,
        // its first share included, at $1 again (Phase 5 review).
        assert_eq!((m.low, m.high), (0, ONE));
        c.check_risk(&mut m, 101 * ONE, ONE, 600_000).unwrap();
        assert_eq!(m.risk, 31 * ONE as u128 * ONE as u128 + ONE as u128 * 400_000);
        assert_eq!((m.low, m.high), (0, ONE));
        assert_eq!(c.total_risk, m.risk);
    }

    /// Phase 4 review: shares burned since the desk's read are not lost
    /// backing. Burned straight through SPL Token after being minted since
    /// the read, they leave the unbacked gap with their risk, as a redemption
    /// takes them: the report leaves backing, risk and prices as they were.
    #[test]
    fn burns_since_the_read_are_not_lost_backing() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        let high = m.high;
        // The desk reads 80 (seen 0, minted 100); 10 are minted after it...
        buy(&mut c, &mut m, 100, 10, 600_000);
        assert_eq!(m.risk, 30 * 400_000);
        // ...and burned outside (supply 100) before the report.
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.high, m.burned_total), (80, 20 * 400_000, high, 10));
        assert_eq!(c.total_risk, m.risk);
        assert_ne!(m.high, ONE);
    }

    /// Phase 5 review (the report gap): shares minted after the desk's read
    /// and redeemed before its report. Their redemption took their risk off
    /// as unhedged shares leaving, and the report must not then take them
    /// off backing too (the gap would grow with no risk for it, and a later
    /// hedge report would take off risk real unhedged shares carry). With the
    /// read's `minted` the report sees the supply never fell below 100 since
    /// the read, so the desk still holds its 80.
    #[test]
    fn a_report_after_shares_minted_and_burned_since_its_read_changes_nothing() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        // 80 hedged: a 20-share gap at 60c carries 20 x 0.4.
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        assert_eq!(m.risk, 20 * 400_000);
        // The desk reads 80 (seen 0, minted 100); 10 are minted and redeemed after it.
        buy(&mut c, &mut m, 100, 10, 600_000);
        c.burned(&mut m, 10, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk), (80, 20 * 400_000));
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.high), (80, 20 * 400_000, 600_000));
        assert_eq!(c.total_risk, m.risk);
        // The same holdings read again: nothing to hedge, nothing taken off.
        c.report(&mut m, 100, 80, 10, 110, 0).unwrap();
        assert_eq!((m.backed, m.risk), (80, 20 * 400_000));
        // Hedging 10 more takes off 10 x 0.4, and the 10 left carry 10 x 0.4.
        c.report(&mut m, 100, 90, 10, 110, 0).unwrap();
        assert_eq!((m.backed, m.risk), (90, 10 * 400_000));
        assert_eq!(c.total_risk, m.risk);
    }

    /// Phase 5 review (hedge_raw): a hedge report with shares minted and
    /// burned since its read is credited with its whole rise: the desk sold
    /// nothing for them.
    #[test]
    fn a_hedge_report_with_round_trips_since_its_read_gets_its_whole_rise() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 50, 0, 100, 0).unwrap();
        assert_eq!(m.risk, 50 * 400_000);
        // The desk reads 70 (seen 0, minted 100); 10 are minted and redeemed after it.
        buy(&mut c, &mut m, 100, 10, 600_000);
        c.burned(&mut m, 10, 100, 0).unwrap();
        assert_eq!(m.risk, 50 * 400_000);
        // Backing 50 -> 70: 20 x 0.4 off, the 30 left carry 30 x 0.4.
        c.report(&mut m, 100, 70, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk), (70, 30 * 400_000));
    }

    /// Phase 5 review (lost_under): backing the desk held below what was
    /// backed is lost in full, whatever burned since its read.
    #[test]
    fn lost_backing_is_not_reduced_by_burns_since_the_read() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        let risk = m.risk;
        // 10 minted and redeemed since the read, then the desk reports 70.
        buy(&mut c, &mut m, 100, 10, 600_000);
        c.burned(&mut m, 10, 100, 0).unwrap();
        assert_eq!(m.risk, risk);
        c.report(&mut m, 100, 70, 0, 100, 0).unwrap();
        // 10 lost at $1 each, and the market is marked unknown.
        assert_eq!((m.backed, m.risk, m.low, m.high), (70, risk + 10 * ONE as u128, 0, ONE));
    }

    /// Phase 6 review (mixed burns): some burns since the read were shares
    /// minted since it, some backed shares. The supply fell to 95 at its
    /// lowest, still above the 80 the desk read, so it sold none of them:
    /// the report is credited with all of its 5 over the 75 left backed, and
    /// the 15 left unhedged carry what the bound lets them (15 x 0.7).
    #[test]
    fn a_report_after_mixed_burns_since_its_read() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 300_000);
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        assert_eq!(m.risk, 14_000_000);
        // Read: seen 0, minted 100. Then 10 minted at 60c and 15 burned.
        buy(&mut c, &mut m, 100, 10, 600_000);
        assert_eq!(m.risk, 18_000_000);
        c.burned(&mut m, 15, 95, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.low_supply), (75, 14_000_000, 95));
        c.report(&mut m, 95, 80, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk), (80, 10_500_000));
        assert_eq!(c.total_risk, m.risk);
    }

    /// Phase 6 review: the paused variant of the report gap. A paused market
    /// takes no quoted mints, but shares minted before the pause may be
    /// redeemed between a read and its report: nothing changes, and the
    /// market is not marked unknown.
    #[test]
    fn a_paused_report_after_round_trips_since_its_read_changes_nothing() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        // Read (seen 0, minted 100); 10 minted, then paused, then redeemed.
        buy(&mut c, &mut m, 100, 10, 600_000);
        m.paused = true;
        c.burned(&mut m, 10, 100, 0).unwrap();
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.low, m.high), (80, 20 * 400_000, 600_000, 600_000));
        assert_eq!(c.total_risk, m.risk);
        // Lost backing on it is marked, and nothing is added until it reopens.
        c.report(&mut m, 100, 70, 10, 110, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.high), (70, 20 * 400_000, ONE));
    }

    /// Probe C of the Phase 6 review: 50 round trips of 100 shares at 50c
    /// between a read and its report. They neither lower backing nor raise
    /// the risk, and a hedge reported from the same read still gets its
    /// whole credit: the round trips are no lever on the risk caps.
    #[test]
    fn round_trips_between_a_read_and_its_report_change_nothing() {
        for direct in [false, true] {
            let mut c = config();
            let mut m = market();
            buy(&mut c, &mut m, 0, 1_000, 500_000);
            c.report(&mut m, 1_000, 900, 0, 1_000, 0).unwrap();
            assert_eq!(m.risk, 50_000_000);
            // The desk reads 900 (seen 0, minted 1,000).
            for _ in 0..50 {
                buy(&mut c, &mut m, 1_000, 100, 500_000);
                if direct {
                    c.burned_outside(&mut m, 1_000);
                } else {
                    c.burned(&mut m, 100, 1_000, 0).unwrap();
                }
            }
            assert_eq!((m.backed, m.risk, m.burned_total), (900, 50_000_000, 5_000));
            let (mut c2, mut m2) = (c.clone(), m.clone());
            c.report(&mut m, 1_000, 900, 0, 1_000, 0).unwrap();
            assert_eq!((m.backed, m.risk, m.high), (900, 50_000_000, 500_000), "direct {direct}");
            assert_eq!(c.total_risk, m.risk);
            // Had the desk bought 50 more before its read: all 50 are credited.
            c2.report(&mut m2, 1_000, 950, 0, 1_000, 0).unwrap();
            assert_eq!((m2.backed, m2.risk), (950, 25_000_000), "direct {direct}");
            assert_eq!(c2.total_risk, m2.risk);
        }
    }

    /// A read before burns of backed shares that came before mints since:
    /// the supply fell to 90, so the desk may have sold 10 of its 100, and
    /// shares minted after that dip are unhedged whatever the totals since
    /// the read say. The report is held to the dip.
    #[test]
    fn a_report_never_counts_shares_the_desk_may_have_sold_at_a_dip() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        // Read: the desk holds 100 (seen 0, minted 100). 10 redeemed: backing 90.
        c.burned(&mut m, 10, 90, 0).unwrap();
        assert_eq!((m.backed, m.low_supply), (90, 90));
        // 10 minted after the dip: they are unhedged and carry 10 x 0.4.
        buy(&mut c, &mut m, 90, 10, 600_000);
        assert_eq!(m.risk, 10 * 400_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk), (90, 10 * 400_000));
    }

    /// A read taken before the last report lands after it: the burns
    /// between that read and the report bound the supply too.
    #[test]
    fn a_read_from_before_the_last_report_is_bounded_by_the_burns_before_it() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        // An old read: the desk holds 100 (seen 0, minted 100). Then 10
        // redeemed, 10 minted, and a newer read of 90 is reported first.
        c.burned(&mut m, 10, 90, 0).unwrap();
        buy(&mut c, &mut m, 90, 10, 600_000);
        c.report(&mut m, 100, 90, 10, 110, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.low_supply, m.burned_at_report), (90, 10 * 400_000, 100, 10));
        // The old read lands: the supply may have been 90 since it, so the
        // desk may hold 90: nothing changes.
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk), (90, 10 * 400_000));
        // Had the newer read found the desk had bought back to 100, the old
        // read's bound (90) is below what is backed: nothing lost, so
        // backing stays (the desk still holds it), and the risk with it.
        let (mut c, mut m) = (config(), market());
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        c.burned(&mut m, 10, 90, 0).unwrap();
        buy(&mut c, &mut m, 90, 10, 600_000);
        c.report(&mut m, 100, 100, 10, 110, 0).unwrap();
        assert_eq!((m.backed, m.risk), (100, 0));
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.high), (100, 0, 600_000));
    }

    /// Phase 7 review (high): a skipped report still books the direct burns
    /// since the last visit, into the market and the total alike (the
    /// caller stores the market whatever `report` returns), and a skip
    /// repeated books nothing more.
    #[test]
    fn a_skipped_report_books_direct_burns_into_the_market_and_the_total_alike() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        // 10 burned outside: the supply is 90 and nothing has booked them.
        for _ in 0..3 {
            assert!(!c.report(&mut m, 90, 0, 11, 100, 0).unwrap());
            assert_eq!((m.burned_total, m.backed, m.risk), (10, 0, 90 * 400_000));
            assert_eq!(c.total_risk, m.risk);
        }
    }

    /// Phase 7 review (burned_at_report): a stale read reported, then
    /// reported again. The desk read 100, the supply dipped to 67 (it sold
    /// 33) and recovered with 50 new shares it has not bought. The first
    /// report is held to the dip; the second must be too, though the
    /// supply since the first report never fell below 117: the burns up
    /// to that report (33) bound a read older than it.
    #[test]
    fn a_stale_read_reported_twice_around_a_dip_is_held_to_the_dip() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 500_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        // Read: the desk holds 100 (seen 0, minted 100).
        c.burned(&mut m, 33, 67, 0).unwrap();
        buy(&mut c, &mut m, 67, 50, 500_000);
        assert_eq!((m.backed, m.risk), (67, 50 * 500_000));
        c.report(&mut m, 117, 100, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk, m.burned_at_report, m.low_supply), (67, 50 * 500_000, 33, 117));
        c.report(&mut m, 117, 101, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.risk), (67, 50 * 500_000));
        assert_eq!(c.total_risk, m.risk);
    }

    /// Phase 8 review: a stale read delivered after a fresh report that
    /// raised backing loses backing, and is still held to the least the
    /// supply can have been since its read. Backed 100 of 100; read A (desk
    /// holds 100); 50 burned directly; 60 minted; a fresh report backs 110;
    /// then A lands. It credits 50 (the supply at the dip), not the 100 it
    /// read. The 10 it held below what was backed are lost at $1 each, and
    /// the market is marked unknown, so the next mint counts the gap at $1.
    #[test]
    fn a_stale_read_after_a_fresh_report_loses_backing_down_to_the_dip() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 500_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        c.burned_outside(&mut m, 50);
        buy(&mut c, &mut m, 50, 60, 500_000);
        c.report(&mut m, 110, 110, 50, 160, 0).unwrap();
        assert_eq!((m.backed, m.burned_at_report), (110, 50));
        let pre = m.risk;
        c.report(&mut m, 110, 100, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.high), (50, ONE));
        assert_eq!(m.risk, pre + 10 * ONE as u128);
        assert_eq!(c.total_risk, m.risk);
    }

    /// Phase 7 review: a direct burn of shares minted at two prices since the
    /// market was last fully backed takes (1 - high) off each, the least any
    /// of them carried, never (1 - low).
    #[test]
    fn a_direct_burn_takes_one_minus_high_off_per_share() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 300_000);
        buy(&mut c, &mut m, 100, 100, 600_000);
        assert_eq!((m.risk, m.low, m.high), (100 * 700_000 + 100 * 400_000, 300_000, 600_000));
        c.burned_outside(&mut m, 190);
        assert_eq!(m.risk, 100 * 700_000 + 90 * 400_000);
        c.refresh(&mut m, 190, 0).unwrap();
        assert_eq!(m.risk, 100 * 700_000 + 90 * 400_000);
        assert_eq!(c.total_risk, m.risk);
        // A redemption of the same shares takes off the same.
        let (mut c2, mut m2) = (config(), market());
        buy(&mut c2, &mut m2, 0, 100, 300_000);
        buy(&mut c2, &mut m2, 100, 100, 600_000);
        c2.burned(&mut m2, 10, 190, 0).unwrap();
        assert_eq!(m2.risk, m.risk);
    }

    #[test]
    fn a_report_skips_reads_against_another_state_and_clamps_to_supply() {
        let mut c = config();
        let mut m = market();
        m.minted_total = 130;
        m.burned_total = 30;
        // Read when 10 had burned and nothing was minted since: the supply
        // has not fallen below 100 since, so the desk sold none of its 100.
        c.report(&mut m, 100, 100, 10, 130, 0).unwrap();
        assert_eq!(m.backed, 100);
        // A read ahead of the burns, or of the mints, or with more burned
        // than minted, is skipped.
        assert!(!c.report(&mut m, 100, 100, 31, 130, 0).unwrap());
        assert!(!c.report(&mut m, 100, 100, 30, 131, 0).unwrap());
        assert!(!c.report(&mut m, 100, 100, 30, 29, 0).unwrap());
        // Above supply is clamped.
        c.report(&mut m, 50, 1_000, 30, 130, 0).unwrap();
        assert_eq!(m.backed, 50);
    }

    #[test]
    fn burns_lower_backing_for_shares_not_minted_since_the_report() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        buy(&mut c, &mut m, 100, 50, 600_000);
        // 70 burn: 50 were minted since the report, 20 were backed.
        c.burned(&mut m, 70, 80, 0).unwrap();
        assert_eq!((m.backed, m.minted_since_report, m.burned_total), (80, 0, 70));
        assert_eq!(m.risk, 0);
        assert_eq!(c.total_risk, 0);
    }

    /// Phase 1 review (high): a holder burns 1 unit straight through SPL Token
    /// on a fully backed market. The next exchange burn used to underflow.
    #[test]
    fn a_direct_burn_on_a_fully_backed_market_never_blocks_a_burn() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        assert_eq!(m.backed, 100);
        // Supply 100 -> 99 outside the program, then a redeem of 10 -> 89.
        c.burned(&mut m, 10, 89, 0).unwrap();
        assert_eq!(m.burned_total, 11);
        assert_eq!(m.burned_total, m.minted_total - 89);
        assert_eq!(m.backed, 89);
        // The same on its own: no underflow even with the books never synced.
        let mut m2 = market();
        m2.backed = 100;
        c.burned(&mut m2, 10, 80, 0).unwrap();
        assert!(m2.backed <= 80);
    }

    /// Phase 6 review: a direct burn is booked as a redemption of the same
    /// shares is, risk included, so a report after either is the same.
    #[test]
    fn direct_burns_lower_backing_or_the_unbacked_count_as_a_redemption_does() {
        let mut c = config();
        let mut m = market();
        // 100 backed, then 50 minted since: 50 unbacked at 60c.
        buy(&mut c, &mut m, 0, 100, 600_000);
        c.report(&mut m, 100, 100, 0, 100, 0).unwrap();
        buy(&mut c, &mut m, 100, 50, 600_000);
        assert_eq!(m.risk, 50 * 400_000);
        let (mut c2, mut m2) = (c.clone(), m.clone());
        // 70 burned outside: 50 leave the unbacked count with their risk, 20 lower backing.
        c.burned_outside(&mut m, 80);
        assert_eq!((m.backed, m.minted_since_report, m.burned_total, m.low_supply), (80, 0, 70, 80));
        assert_eq!((m.risk, c.total_risk), (0, 0));
        // The same 70 redeemed: the same books.
        c2.burned(&mut m2, 70, 80, 0).unwrap();
        assert_eq!((m2.backed, m2.minted_since_report, m2.burned_total, m2.risk, m2.low_supply), (80, 0, 70, 0, 80));
        // Seen again, nothing more happens.
        c.burned_outside(&mut m, 80);
        assert_eq!((m.backed, m.burned_total), (80, 70));
    }

    /// Phase 3 review: backing must fall for direct burns beyond the mints
    /// since the report, even when it stays under the supply, or the gap the
    /// next mint is bounded by is too small.
    #[test]
    fn direct_burns_beyond_the_mints_since_the_report_lower_backing() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        // The desk holds 80 of 100: a 20-share gap, none minted since.
        c.report(&mut m, 100, 80, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.minted_since_report, m.risk), (80, 0, 20 * 400_000));
        // 10 burned outside: all backed, so backing drops to 70 (the gap stays 20).
        c.burned_outside(&mut m, 90);
        assert_eq!((m.backed, m.burned_total), (70, 10));
        // The bound at the new supply keeps the 20-share gap's risk...
        c.refresh(&mut m, 90, 0).unwrap();
        assert_eq!(m.risk, 20 * 400_000);
        // ...and the next mint is checked against it.
        c.check_risk(&mut m, 90, 10, 600_000).unwrap();
        assert_eq!(m.risk, 30 * 400_000);
    }

    #[test]
    fn a_report_counts_direct_burns_since_the_keepers_read() {
        let mut c = config();
        let mut m = market();
        buy(&mut c, &mut m, 0, 100, 600_000);
        // The desk read 100 shares (seen 0, minted 100); then 30 were burned
        // outside: it may have sold 30.
        c.report(&mut m, 70, 100, 0, 100, 0).unwrap();
        assert_eq!((m.backed, m.burned_total), (70, 30));
        // A keeper that already counted them (seen = minted_total - supply) is not skipped.
        assert!(c.report(&mut m, 70, 70, 30, 100, 0).unwrap());
    }

    #[test]
    fn settled_market_drops_its_risk_at_the_next_burn() {
        let mut c = config();
        let mut m = market();
        c.check_risk(&mut m, 0, 100 * ONE, 600_000).unwrap();
        m.settle_at = 10;
        c.burned(&mut m, 0, 100 * ONE, 10).unwrap();
        assert_eq!((m.risk, c.total_risk), (0, 0));
    }
}
