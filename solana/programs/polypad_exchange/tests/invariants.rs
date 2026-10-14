//! Stateful fuzzing on LiteSVM: the intent of contracts/test's
//! Queue.invariants, fuzz/SystemInvariants and V12's
//! testFuzz_totalRiskIsTheSumOfMarkets, for the Solana program. Random actors
//! mint, redeem, burn straight through SPL Token, send shares to the
//! exchange; the keeper reports, settles, pauses and bridges; the owner
//! cancels, halts and resumes; time passes; recipients are frozen; clients
//! pass queue lists read some steps ago, or none. After every step the
//! spec's invariants are checked against an independent model of what every
//! actor was promised and of the least risk each market must carry, and the
//! run ends by unwinding everything and checking everyone is paid exactly
//! that. The model prices with the spec's formulas, never the program's own
//! math, and every refusal must be one the action may give in the state it
//! was tried in.
//!
//! Deterministic: `PX_FUZZ_SEED` (default 1) picks the first seed,
//! `PX_FUZZ_RUNS` (default 8) how many seeds, `PX_FUZZ_STEPS` (default 400)
//! the steps per run. Run `anchor build` first.

mod common;

use std::collections::HashMap;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::AccountMeta;
use anchor_spl::token::spl_token::error::TokenError;
use common::*;
use solana_instruction_error::InstructionError;
use solana_signer::Signer;
use solana_transaction_error::TransactionError;

/// splitmix64: small, fast, and the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

const ACTORS: usize = 4;
const MARKETS: usize = 3;
const START_USDC: u64 = 50_000 * USDC;

/* ---------------------------------------------------------------- model */

// The fees the fuzz sets, all different, so pricing a buy with the sell
// spread, or a payout with either, shows in the ledger.
const BUY_SPREAD: u16 = 25;
const SELL_SPREAD: u16 = 40;
const SETTLE_FEE: u16 = 75;
const MIN_PRICE: u64 = 10_000;
const MAX_PRICE: u64 = 990_000;
/// $1 per share, and a basis point, as the spec defines them.
const DOLLAR: u128 = 1_000_000;
const BPS_: u128 = 10_000;

/// EVM-PORT-SPEC `_mintOut`: floor(usdcIn * ONE / ceil(price * (BPS + spread) / BPS)).
fn model_mint_out(price: u64, usdc_in: u64) -> u64 {
    let eff = (price as u128 * (BPS_ + BUY_SPREAD as u128)).div_ceil(BPS_);
    if eff == 0 {
        return 0;
    }
    (usdc_in as u128 * DOLLAR / eff) as u64
}

/// EVM-PORT-SPEC `_redeemOut`: floor(amountIn * floor(price * (BPS - fee) / BPS) / ONE).
fn model_redeem_out(price: u64, amount: u64, fee_bps: u16) -> u64 {
    let eff = price as u128 * (BPS_ - fee_bps as u128) / BPS_;
    (amount as u128 * eff / DOLLAR) as u64
}

/// A payout is in effect once its time has come.
fn settled(mk: &Market, now: i64) -> bool {
    mk.settle_at != 0 && now >= mk.settle_at
}

/// A mint goes at the payout once settled, and at once for a pending $1 payout.
fn at_payout(mk: &Market, now: i64) -> bool {
    settled(mk, now) || (mk.settle_at != 0 && mk.payout as u128 == DOLLAR)
}

fn ex(e: ExchangeError) -> u32 {
    code(e)
}

/// SPL Token's refusal to move USDC from or to a frozen account.
const FROZEN: u32 = TokenError::AccountFrozen as u32;

/// The desk's read of a market's holdings: the view's counts then, and the
/// model's own state then.
#[derive(Clone, Copy)]
struct Read {
    supply: u64,
    seen: u64,
    minted: u64,
    /// The model's `burns` and `backing` then.
    burns: u64,
    backing: u64,
    /// The lowest supply the model has seen since, the read's included.
    low: u64,
    /// Reports made before it (`report_seq`).
    seq: u64,
    /// Shares minted after it were burned straight through SPL Token.
    direct: bool,
}

struct World {
    env: Env,
    rng: Rng,
    markets: Vec<Mkt>,
    actors: Vec<User>,
    /// What each actor was promised by every sale that went through, at the
    /// model's own price math, less sales the owner cancelled.
    promised: Vec<u64>,
    /// USDC each actor paid into mints.
    spent: Vec<u64>,
    /// USDC that came into the float from outside, and went to the bridge.
    funded: u64,
    bridged: u64,
    /// The keeper's last holdings read per market, reported later; and an
    /// older one that may land after it (and after its report).
    reads: Vec<Option<Read>>,
    old_reads: Vec<Option<Read>>,
    /// The lowest supply since each market's last report, and every share
    /// burned by then (the spec's `low_supply`, `burned_at_report`), and that
    /// report's number (`report_seq`).
    low: Vec<u64>,
    burned_at_report: Vec<u64>,
    reported_at: Vec<u64>,
    /// Reports made so far, all markets.
    report_seq: u64,
    frozen: Vec<bool>,
    /// Per market, a lower bound on the quoted-mint shares nobody hedged:
    /// each quoted mint adds its shares, every share that leaves the supply
    /// is taken as one of them, and a report raises it only by lost backing
    /// and otherwise can only lower it, to the supply it leaves unhedged less
    /// `riskless`. Each of them carries at least
    /// (ONE - `hi`), `hi` the highest price a quoted mint on the market paid
    /// since `unbacked` was last 0, so until the market settles its stored
    /// risk may never be below `unbacked * (ONE - hi)`.
    unbacked: Vec<u64>,
    hi: Vec<u64>,
    /// Shares minted at a pending $1 payout (the owner may then cancel it):
    /// they sit in the unbacked gap carrying no risk, as they cost the float
    /// nothing whatever the outcome. A hedge report raises backing for any
    /// share alike (shares are fungible: whichever the desk buys, the loss
    /// at a $1 outcome falls by the same), so the quoted shares left
    /// unhedged number at most the gap less these. The model's clamp would
    /// otherwise count them as quoted shares and demand risk the program
    /// rightly never charged: a gap in the model, not the program.
    riskless: Vec<u64>,
    /// Backing lost on a live market since its last quoted mint found it
    /// fully backed: the program must hold `high == ONE` exactly then, and
    /// each quoted mint while it is set counts the whole gap at $1.
    unknown: Vec<bool>,
    /// Supply at the end of the last step, to see shares leave it.
    supplies: Vec<u64>,
    /// Every share that ever left each market's supply, however it left.
    burns: Vec<u64>,
    /// The backing each market must hold once it has seen every burn: the
    /// last report (no more than the lowest supply since its read, and,
    /// nothing lost, no less than what was backed), then lowered by each
    /// share that leaves the supply beyond `fresh`, the quoted mints since
    /// that report (those leave the unbacked gap instead).
    backing: Vec<u64>,
    fresh: Vec<u64>,
    /// Queue triples as a client read them some steps ago.
    old_queue: Option<Vec<AccountMeta>>,
    /// The list just handed to the next instruction is not empty yet lacks
    /// the live head: only the program's head rule can refuse it. Names what
    /// sits where the head was: claims paid and closed only, or a claim set
    /// aside (which must no more pass for the head than a closed one).
    headless: Option<&'static str>,
    /// The next queue list races whenever the queue is deep enough.
    force_race: bool,
    /// The next prices are inside the band.
    in_band: bool,
    /// How often the run reached each dangerous state (the coverage gate).
    hits: HashMap<&'static str, u64>,
    /// States the action being tried reaches if it goes through.
    pending: Vec<&'static str>,
    steps: u64,
    unwinding: bool,
    /// The last action tried, for failure messages.
    last: &'static str,
    counts: HashMap<&'static str, (u64, u64)>,
    /// Refusals by action and error code (printed with `PX_FUZZ_VERBOSE`).
    refusals: HashMap<(&'static str, u32), u64>,
}

impl World {
    fn new(seed: u64) -> World {
        let mut env = Env::new();
        // Caps that bind now and then: queues, delayed sales and risk refusals all happen.
        env.owner_ix(px::instruction::SetParams {
            buy_spread_bps: BUY_SPREAD,
            sell_spread_bps: SELL_SPREAD,
            min_price: MIN_PRICE,
            max_price: MAX_PRICE,
            default_max_risk: 3_000 * USDC,
        })
        .unwrap();
        env.owner_ix(px::instruction::SetSettleFee { settle_fee_bps: SETTLE_FEE }).unwrap();
        env.owner_ix(px::instruction::SetMaxTotalRisk { cap: 6_000 * USDC }).unwrap();
        env.owner_ix(px::instruction::SetOutflowCap { floor: 400 * USDC, float_bps: 1_000 }).unwrap();
        let markets: Vec<Mkt> = (0..MARKETS).map(|i| env.create_market(pid(10 + i as u8))).collect();
        let actors: Vec<User> = (0..ACTORS).map(|_| env.user(START_USDC)).collect();
        for a in &actors {
            for m in &markets {
                env.ptoken_account(a, m);
            }
        }
        for m in &markets {
            env.exchange_ata(m);
        }
        World {
            env,
            rng: Rng(seed),
            markets,
            actors,
            promised: vec![0; ACTORS],
            spent: vec![0; ACTORS],
            funded: 0,
            bridged: 0,
            reads: vec![None; MARKETS],
            old_reads: vec![None; MARKETS],
            low: vec![0; MARKETS],
            burned_at_report: vec![0; MARKETS],
            reported_at: vec![0; MARKETS],
            report_seq: 0,
            frozen: vec![false; ACTORS],
            unbacked: vec![0; MARKETS],
            hi: vec![0; MARKETS],
            riskless: vec![0; MARKETS],
            unknown: vec![false; MARKETS],
            supplies: vec![0; MARKETS],
            burns: vec![0; MARKETS],
            backing: vec![0; MARKETS],
            fresh: vec![0; MARKETS],
            old_queue: None,
            headless: None,
            force_race: false,
            in_band: false,
            hits: HashMap::new(),
            pending: vec![],
            steps: 0,
            unwinding: false,
            last: "",
            counts: HashMap::new(),
            refusals: HashMap::new(),
        }
    }

    /// Record an action's outcome. A refusal is fine only with one of
    /// `allowed`, the codes this action may give in the state it was tried
    /// in (the fuzz tries things that should be refused); any other code, or
    /// a runtime failure, is a bug.
    fn outcome(&mut self, name: &'static str, r: &Res, allowed: &[u32]) -> bool {
        self.last = name;
        let pending = std::mem::take(&mut self.pending);
        match r {
            Ok(()) => {
                self.counts.entry(name).or_default().0 += 1;
                for what in pending {
                    self.hit(what);
                }
                true
            }
            Err(err) => {
                self.counts.entry(name).or_default().1 += 1;
                match err {
                    TransactionError::InstructionError(_, InstructionError::Custom(c)) => {
                        *self.refusals.entry((name, *c)).or_default() += 1;
                        assert!(allowed.contains(c), "{name}: refused with {c}, not one of {allowed:?}, at step {}", self.steps);
                        if *c == ex(ExchangeError::MissingAccount) {
                            if let Some(what) = self.headless.take() {
                                self.hit(what);
                            }
                        }
                    }
                    other => panic!("{name}: {other:?} at step {}", self.steps),
                }
                false
            }
        }
    }

    fn hit(&mut self, what: &'static str) {
        *self.hits.entry(what).or_default() += 1;
    }

    /// The queue triples the next mint, sale or release passes: usually the
    /// current ones; sometimes a list read some steps ago, or read just now
    /// and then raced by another transaction paying the queue (claims on it
    /// paid or set aside since), now and then cut to the claim that race
    /// took, which leaves a closed or set-aside account where the head was;
    /// sometimes none. `race` is the percent of tries that race, when the
    /// queue is deep enough. Returns whether the list lacks the head of a
    /// live queue, the one case `MissingAccount` is allowed.
    fn queue_list(&mut self, race: u64) -> bool {
        // Claims behind the head, so the queue is still live once it is paid.
        let deep = {
            let c = self.env.config();
            c.claim_head + 1 < c.claim_tail
        };
        self.headless = None;
        let roll = if std::mem::take(&mut self.force_race) { 24 } else { self.rng.below(100) };
        let list = match roll {
            0..=19 => self.old_queue.clone(),
            20..=23 => Some(vec![]),
            n if deep && n < 24 + race => {
                let mut read = self.env.queue_metas(QUEUE_BATCH);
                // Someone else's mint brings the USDC, then pays the queue.
                let head_addr = pda(&[CLAIM_SEED, &self.env.config().claim_head.to_le_bytes()]);
                let head = self.env.claim_at(&head_addr).unwrap();
                self.env.fund_vault(head.amount);
                self.funded += head.amount;
                // Its recipient's account is frozen now and then just
                // before, so the claim is set aside rather than paid.
                if self.rng.chance(35) {
                    let a = self.actor_of(&head.to).unwrap();
                    if !self.frozen[a] {
                        self.frozen[a] = true;
                        self.env.freeze(&head.to, true);
                    }
                }
                let allowed = if self.env.config().halted { vec![ex(ExchangeError::Halted)] } else { vec![] };
                let r = self.env.pay_queue(1);
                self.outcome("pay_queue", &r, &allowed);
                // A client that offered only the head it read: that account is
                // closed or set aside now, and must not pass for the new head.
                // Set aside is the rarer outcome, so that list is cut more often.
                let aside = self.env.claim_at(&head_addr).is_some_and(|cl| cl.set_aside);
                if self.rng.chance(if aside { 50 } else { 30 }) {
                    read.truncate(3);
                }
                Some(read)
            }
            _ => None,
        };
        let Some(list) = list else { return false };
        let c = self.env.config();
        let lacks = c.claim_head < c.claim_tail && {
            let head = pda(&[CLAIM_SEED, &c.claim_head.to_le_bytes()]);
            !list.chunks(3).take(QUEUE_BATCH).any(|t| t[0].pubkey == head)
        };
        if !lacks && c.claim_head < c.claim_tail {
            // What the program must skip on this list, for the coverage gate.
            for t in list.chunks(3) {
                match self.env.claim_at(&t[0].pubkey) {
                    None => self.pending.push("stale list: a paid claim skipped"),
                    Some(cl) if cl.set_aside => self.pending.push("stale list: a set-aside claim skipped"),
                    _ => {}
                }
            }
        }
        if lacks && !list.is_empty() {
            let aside = list.chunks(3).any(|t| self.env.claim_at(&t[0].pubkey).is_some_and(|cl| cl.set_aside));
            self.headless = Some(if aside { "a list with a set-aside claim but not the head refused" } else { "a list of paid claims but not the head refused" });
        }
        *self.env.queue_list.borrow_mut() = Some(list);
        lacks
    }

    fn actor_of(&self, usdc: &Pubkey) -> Option<usize> {
        self.actors.iter().position(|a| a.usdc == *usdc)
    }

    fn ptoken(&self, a: usize, m: usize) -> Pubkey {
        Pubkey::find_program_address(&[self.actors[a].key().as_ref(), self.markets[m].mint.as_ref()], &px::ID).0
    }

    fn price(&mut self) -> u64 {
        // Mostly inside the band, now and then just outside it.
        match if self.in_band { 9 } else { self.rng.below(10) } {
            0 => 5_000,
            1 => 995_000,
            _ => MIN_PRICE + self.rng.below(MAX_PRICE - MIN_PRICE + 1),
        }
    }

    /// Whether a quoted mint of `out` shares at `price` on market `m` (state
    /// `mk` before it) can pass a risk cap. The most the market can carry
    /// after it is what it carries now, or every unbacked share at $1 if its
    /// backing was lost (backing then as low as the direct burns since the
    /// last look can take it), plus the new shares at (ONE - price) each.
    fn risk_cap_can_bind(&self, m: usize, mk: &Market, out: u64, price: u64) -> bool {
        let c = self.env.config();
        let supply = self.env.supply(&self.markets[m].mint);
        let unseen = mk.minted_total.saturating_sub(supply).saturating_sub(mk.burned_total);
        let carried = if mk.high == ONE {
            (supply - mk.backed.saturating_sub(unseen).min(supply)) as u128 * DOLLAR
        } else {
            mk.risk
        };
        let most = carried + out as u128 * (DOLLAR - price as u128);
        most / DOLLAR > c.max_risk(mk) as u128 || (c.total_risk - mk.risk + most) / DOLLAR > c.max_total_risk as u128
    }

    /* ------------------------------------------------------------ actions */

    fn mint(&mut self) {
        let (a, m) = (self.rng.below(ACTORS as u64) as usize, self.rng.below(MARKETS as u64) as usize);
        self.mint_of(a, m);
    }

    fn mint_of(&mut self, a: usize, m: usize) {
        let usdc_in = 1 + self.rng.below(2_000 * USDC);
        self.mint_usdc(a, m, usdc_in);
    }

    fn mint_usdc(&mut self, a: usize, m: usize, usdc_in: u64) {
        let mk = self.env.market(&self.markets[m]);
        let at_payout = at_payout(&mk, self.env.now);
        let at_pending_one = at_payout && !settled(&mk, self.env.now);
        let price = if at_payout { mk.payout } else { self.price() };
        let want = model_mint_out(price, usdc_in);
        let before = self.env.balance(&self.ptoken(a, m));
        let over = self.backed_over_supply(m);
        let lacks_head = self.queue_list(30);
        let mut allowed = vec![];
        if self.env.config().halted {
            allowed.push(ex(ExchangeError::Halted));
        }
        if self.frozen[a] {
            allowed.push(FROZEN);
        }
        if want == 0 {
            allowed.push(ex(ExchangeError::ZeroAmount));
        }
        if lacks_head {
            allowed.push(ex(ExchangeError::MissingAccount));
        }
        if at_payout {
            if price == 0 {
                allowed.push(ex(ExchangeError::SettledAtZero));
            }
        } else {
            if !(MIN_PRICE..=MAX_PRICE).contains(&price) {
                allowed.push(ex(ExchangeError::PriceOutOfBand));
            }
            if mk.paused {
                allowed.push(ex(ExchangeError::MarketPaused));
            }
            if self.risk_cap_can_bind(m, &mk, want, price) {
                allowed.push(ex(ExchangeError::RiskCap));
            }
        }
        let r = if at_payout && self.rng.chance(50) {
            let (u, mm) = (&self.actors[a], &self.markets[m]);
            let u = User { kp: Env::as_kp(&u.kp), usdc: u.usdc };
            self.env.mint_at_payout(&u, mm, usdc_in)
        } else {
            let u = User { kp: Env::as_kp(&self.actors[a].kp), usdc: self.actors[a].usdc };
            self.env.buy(&u, &self.markets[m], usdc_in, price)
        };
        if self.outcome("mint", &r, &allowed) {
            // Every mint must offer a live queue's head.
            assert!(!lacks_head, "a mint went through without the queue head");
            self.spent[a] += usdc_in;
            let got = self.env.balance(&self.ptoken(a, m)) - before;
            assert_eq!(got, want, "mint paid out the wrong shares");
            let (c, mk) = (self.env.config(), self.env.market(&self.markets[m]));
            if !at_payout {
                assert!(mk.risk / ONE as u128 <= c.max_risk(&mk) as u128, "market risk over its cap after a quoted mint");
                assert!(c.total_risk / ONE as u128 <= c.max_total_risk as u128, "total risk over its cap after a quoted mint");
                // Backing lost since the market was last fully backed (marked
                // unknown): every share in the gap the mint found counts at $1,
                // whatever it was minted at, and the new ones at (ONE - price).
                // Every later mint too, until one finds the market fully backed.
                let supply = self.env.supply(&self.markets[m].mint) - got;
                let fresh_book = got as u128 * (DOLLAR - price as u128);
                // No unhedged quoted share left before this mint: the floor's
                // price starts again at this one, as the program's `high`
                // does on a fully backed market. (A floor at the highest
                // price ever would be all but 0 after a few mints near $1.)
                let restart = self.unbacked[m] == 0;
                if supply <= mk.backed {
                    // Fully backed: the books start again at this mint's price.
                    self.unknown[m] = false;
                    assert_eq!((mk.risk, mk.low, mk.high), (fresh_book, price, price), "market {m}: a mint on a fully backed market did not start the books again, at step {}", self.steps);
                } else if self.unknown[m] {
                    let want = (supply - mk.backed) as u128 * DOLLAR + fresh_book;
                    assert_eq!(mk.risk, want, "market {m}: a mint after lost backing did not count the gap at $1, at step {}", self.steps);
                    assert_eq!((mk.low, mk.high), (0, ONE), "market {m}: a mint on a market marked unknown cleared the mark, at step {}", self.steps);
                    self.hit("a quoted mint on a market marked unknown");
                    if mk.minted_since_report > got {
                        self.hit("a second quoted mint on a market marked unknown");
                    }
                }
                self.unbacked[m] += got;
                self.hi[m] = if restart { price } else { self.hi[m].max(price) };
                self.fresh[m] += got;
            }
            if at_pending_one {
                self.riskless[m] += got;
            }
            if over {
                self.hit("an instruction while backed > supply");
            }
            if at_payout {
                self.touched(m);
            } else {
                self.synced(m);
            }
        }
    }

    fn redeem(&mut self) {
        let (a, m) = (self.rng.below(ACTORS as u64) as usize, self.rng.below(MARKETS as u64) as usize);
        self.redeem_of(a, m);
    }

    fn redeem_of(&mut self, a: usize, m: usize) {
        self.redeem_up_to(a, m, u64::MAX);
    }

    /// A sale of at most `cap` of the actor's shares.
    fn redeem_up_to(&mut self, a: usize, m: usize, cap: u64) {
        let bal = self.env.balance(&self.ptoken(a, m)).min(cap);
        if bal == 0 {
            return;
        }
        let mk = self.env.market(&self.markets[m]);
        let settled = settled(&mk, self.env.now);
        let (price, fee) = if settled { (mk.payout, SETTLE_FEE) } else { (self.price(), SELL_SPREAD) };
        let over = self.backed_over_supply(m);
        let lacks_head = self.queue_list(30);
        let c = self.env.config();
        // This hour's budget, read after any race above moved the float.
        let metered = settled || c.meter_sales;
        let remaining = if metered { c.outflow_remaining(self.env.now, self.env.balance(&self.env.vault.clone())) } else { u64::MAX };
        // Now and then a sale sized to pass a budget not yet spent by under
        // $1, which is paid now in full rather than ticketed.
        let eff = price as u128 * (BPS_ - fee as u128) / BPS_;
        let amount = if metered && remaining > 0 && eff > 0 && self.rng.chance(20) {
            // Over by under MIN_DELAYED, or right at its edge either side.
            let over = match self.rng.below(4) {
                0 => MIN_DELAYED - 1,
                1 => MIN_DELAYED,
                2 => MIN_DELAYED + 1,
                _ => 1 + self.rng.below(MIN_DELAYED - 1),
            };
            let target = remaining.saturating_add(over);
            ((target as u128 * DOLLAR).div_ceil(eff).min(bal as u128) as u64).max(1)
        } else {
            1 + self.rng.below(bal)
        };
        let out = model_redeem_out(price, amount, fee);
        // Under MIN_DELAYED over a metered cap: refused once this hour's
        // budget is spent, paid now in full while any of it is left.
        let dust_refused = out > 0 && out < MIN_DELAYED && remaining == 0;
        let dust_paid = remaining > 0 && out > remaining && out - remaining < MIN_DELAYED;
        let mut allowed = vec![];
        if dust_refused {
            allowed.push(ex(ExchangeError::DustOverCap));
        }
        if c.halted {
            allowed.push(ex(ExchangeError::Halted));
        }
        if self.frozen[a] {
            allowed.push(FROZEN);
        }
        if lacks_head {
            allowed.push(ex(ExchangeError::MissingAccount));
        }
        let (delayed, tail) = (c.delayed_next, c.claim_tail);
        let used = c.outflow_used(self.env.now);
        let usdc = self.env.balance(&self.actors[a].usdc);
        let u = User { kp: Env::as_kp(&self.actors[a].kp), usdc: self.actors[a].usdc };
        let r = if settled {
            self.env.redeem_at_payout(&u, &self.markets[m], amount)
        } else {
            self.env.sell(&u, &self.markets[m], amount, price)
        };
        if dust_refused && matches!(r, Err(TransactionError::InstructionError(_, InstructionError::Custom(c))) if c == ex(ExchangeError::DustOverCap)) {
            self.hit("dust over a spent cap refused");
        }
        if self.outcome("redeem", &r, &allowed) {
            assert!(!dust_refused, "a sale of under $1 went through over a spent cap at step {}", self.steps);
            let after = self.env.config();
            let ticketed = if after.delayed_next > delayed { self.env.ticket_at(delayed).map_or(0, |t| t.amount) } else { 0 };
            if dust_paid {
                assert_eq!(after.delayed_next, delayed, "under $1 over a cap not yet spent was ticketed, not paid now");
                self.hit("under $1 over the cap paid now");
            } else if metered && out > remaining {
                // MIN_DELAYED or more over: exactly the excess waits.
                assert_eq!(ticketed, out - remaining, "a sale {} over the cap was not ticketed for exactly that, at step {}", out - remaining, self.steps);
                if remaining > 0 && out - remaining == MIN_DELAYED {
                    self.hit("exactly MIN_DELAYED over the cap ticketed");
                }
            }
            // The meter counts exactly what was paid now (dust over the cap
            // included); only a ticket's part waits unmetered.
            let want_used = if metered { used + (out - ticketed) } else { used };
            assert_eq!(after.outflow_used(self.env.now), want_used, "the hour's outflow did not count what the sale paid now, at step {}", self.steps);
            // A sale that paid now or queued ran the queue first, so it must
            // have offered a live queue's head; only a $0 or wholly delayed
            // sale pays nothing and may go through without it.
            let paid = self.env.balance(&self.actors[a].usdc) > usdc || self.env.config().claim_tail > tail;
            assert!(!(lacks_head && paid), "a sale paid without the queue head");
            self.promised[a] += out;
            if settled {
                self.hit("a redeem at the payout");
            }
            if self.env.config().delayed_next > delayed {
                self.hit("a sale delayed");
            }
            if over {
                self.hit("an instruction while backed > supply");
            }
            self.touched(m);
        }
    }

    fn burn_direct(&mut self) {
        let (a, m) = (self.rng.below(ACTORS as u64) as usize, self.rng.below(MARKETS as u64) as usize);
        let bal = self.env.balance(&self.ptoken(a, m));
        if bal == 0 {
            return;
        }
        // Dust most of the time (the cheap attack), sometimes a real amount.
        let amount = if self.rng.chance(70) { 1 + self.rng.below(3) } else { 1 + self.rng.below(bal) }.min(bal);
        self.burn_direct_of(a, m, amount);
        if self.rng.chance(50) {
            self.book_direct_burns(m);
        }
    }

    /// An instruction books the direct burns pending on market `m` (a
    /// refresh of it alone). Independently of the program's own math: of
    /// the `n` shares burned, at most the quoted mints since the report
    /// leave the unbacked count, and each takes off no more than
    /// (1 - high), the least any share minted since the market was last
    /// fully backed carried; only the bound at the lower supply may take
    /// more. Exactly that, as a redemption of the same shares would.
    fn book_direct_burns(&mut self, m: usize) {
        let mm = Env::market_keys(self.markets[m].id);
        let pre = self.env.market(&mm);
        let supply = self.env.supply(&mm.mint);
        let n = pre.minted_total - supply - pre.burned_total;
        let r = self.env.refresh_risk(&[&mm]);
        assert!(self.outcome("refresh_risk", &r, &[]));
        let mk = self.env.market(&mm);
        if !settled(&mk, self.env.now) {
            let per = DOLLAR - pre.high.min(ONE) as u128;
            let bound = if supply <= mk.backed { 0 } else { (supply - mk.backed) as u128 * (DOLLAR - pre.low as u128) };
            let most = pre.minted_since_report.min(n) as u128 * per;
            assert!(
                mk.risk >= pre.risk.saturating_sub(most).min(bound),
                "market {m}: booking {n} directly burned shares took more than (1 - high) off each ({} -> {}) at step {}",
                pre.risk,
                mk.risk,
                self.steps
            );
            let gap = (supply + n).saturating_sub(pre.backed);
            let k = pre.minted_since_report.min(n).min(gap);
            assert_eq!(mk.risk, pre.risk.saturating_sub(k as u128 * per).min(bound), "market {m}: booking a direct burn left the wrong risk at step {}", self.steps);
            if k > 0 && pre.low < pre.high && pre.high < ONE && mk.risk > 0 {
                self.hit("a direct burn booked on a market minted at more than one price");
            }
        }
        self.touched(m);
    }

    /// Reports the program must skip (a read ahead of the market's counts,
    /// as from a keeper bug or a forked slot), often for one market several
    /// times in one batch and with a direct burn of shares minted since its
    /// report pending. A skip changes nothing but booking those burns, into
    /// the market and the total alike: the books stay synced and the total
    /// the sum of the markets (checked after every step).
    fn skipped_reports(&mut self) {
        let m = self.rng.below(MARKETS as u64) as usize;
        let a = self.unfrozen();
        if self.rng.chance(85) {
            // Mostly shares just minted, so the burn takes risk off as it is booked.
            let before = self.env.balance(&self.ptoken(a, m));
            if self.rng.chance(70) {
                self.in_band = true;
                let usdc_in = 1 + self.rng.below(300 * USDC);
                self.mint_usdc(a, m, usdc_in);
                self.in_band = false;
                self.left_supply();
            }
            let bal = self.env.balance(&self.ptoken(a, m));
            let got = bal - before;
            let n = if got > 0 && self.rng.chance(80) { 1 + self.rng.below(got) } else { 1 + self.rng.below(bal.max(1)) }.min(bal);
            if n > 0 {
                self.burn_direct_of(a, m, n);
                self.left_supply();
            }
        }
        let n = 1 + self.rng.below(8) as usize;
        let mut picks = vec![];
        for _ in 0..n {
            picks.push(if self.rng.chance(75) { m } else { self.rng.below(MARKETS as u64) as usize });
        }
        let mut entries = vec![];
        for &i in &picks {
            let v = self.env.view_market(&Env::market_keys(self.markets[i].id));
            let (seen, minted) = match self.rng.below(3) {
                1 => (v.burned_total, v.minted_total + 1 + self.rng.below(10 * USDC)),
                2 if v.burned_total > 0 => (v.burned_total, v.burned_total - 1 - self.rng.below(v.burned_total)),
                _ => (v.burned_total + 1 + self.rng.below(10 * USDC), v.minted_total),
            };
            entries.push((i, self.rng.below(2 * v.supply + 1), seen, minted));
        }
        let pre: Vec<Market> = self.markets.iter().map(|mk| self.env.market(mk)).collect();
        let pending: Vec<bool> = (0..MARKETS).map(|i| pre[i].burned_total + self.env.supply(&self.markets[i].mint) < pre[i].minted_total).collect();
        let keys: Vec<Mkt> = self.markets.iter().map(|mk| Env::market_keys(mk.id)).collect();
        let batch: Vec<(&Mkt, u64, u64, u64)> = entries.iter().map(|&(i, a, s, mi)| (&keys[i], a, s, mi)).collect();
        let r = self.env.report_batch(&batch);
        assert!(self.outcome("report", &r, &[]));
        for i in 0..MARKETS {
            if !picks.contains(&i) {
                continue;
            }
            let mk = self.env.market(&self.markets[i]);
            if !pending[i] {
                // Nothing to book: a skip leaves the market as it was.
                let p = &pre[i];
                assert_eq!(
                    (mk.backed, mk.risk, mk.low, mk.high, mk.minted_since_report, mk.burned_total, mk.burned_at_report),
                    (p.backed, p.risk, p.low, p.high, p.minted_since_report, p.burned_total, p.burned_at_report),
                    "market {i}: a skipped report changed the market at step {}",
                    self.steps
                );
            } else if mk.risk < pre[i].risk {
                self.hit("a skipped report with a direct burn pending");
                if picks.iter().filter(|&&j| j == i).count() > 1 {
                    self.hit("a skipped report batch with one market twice and a burn pending");
                }
            }
            // Synced, not `touched`: a skip need not drop a settled market's risk.
            self.synced(i);
        }
    }

    fn burn_direct_of(&mut self, a: usize, m: usize, amount: u64) {
        let u = User { kp: Env::as_kp(&self.actors[a].kp), usdc: self.actors[a].usdc };
        let r = self.env.burn_direct(&u, &self.markets[m], amount);
        assert!(self.outcome("burn_direct", &r, &[]));
        if amount > self.fresh[m] && self.backing[m] > 0 {
            self.hit("a direct burn beyond the mints since the report");
        }
    }

    /// Shares sent to the exchange (its `held` account or its associated
    /// token account), then absorbed.
    fn absorb(&mut self) {
        let (a, m) = (self.rng.below(ACTORS as u64) as usize, self.rng.below(MARKETS as u64) as usize);
        let bal = self.env.balance(&self.ptoken(a, m));
        let mm = Env::market_keys(self.markets[m].id);
        let ata = self.env.exchange_ata(&mm);
        if bal > 0 && self.rng.chance(70) {
            let dest = if self.rng.chance(50) { mm.held } else { ata };
            let u = User { kp: Env::as_kp(&self.actors[a].kp), usdc: self.actors[a].usdc };
            let r = self.env.transfer_ptoken(&u, &mm, dest, 1 + self.rng.below(bal.min(50 * USDC)));
            assert!(self.outcome("send_to_exchange", &r, &[]));
        }
        let other = if self.rng.chance(60) { Some(ata) } else { None };
        let over = self.backed_over_supply(m);
        let supply = self.env.supply(&mm.mint);
        let r = self.env.absorb_with(&mm, other);
        assert!(self.outcome("absorb", &r, &[]));
        let mk = self.env.market(&mm);
        assert!(self.env.balance(&mm.held) == mk.delayed_shares, "absorb left stray shares in held");
        if other.is_some() {
            assert_eq!(self.env.balance(&ata), 0, "absorb left shares in the exchange's ATA");
        }
        if over {
            self.hit("an instruction while backed > supply");
        }
        // Only an absorb that burned something is a burn.
        if self.env.supply(&mm.mint) < supply {
            self.touched(m);
        } else {
            self.synced(m);
        }
    }

    /// The desk reads a market now: the view's counts.
    fn read_now(&mut self, m: usize) -> Read {
        let v = self.env.view_market(&Env::market_keys(self.markets[m].id));
        assert_eq!(v.burned_total, self.burns[m], "market {m}: the view's burns are not every share that left the supply");
        Read { supply: v.supply, seen: v.burned_total, minted: v.minted_total, burns: self.burns[m], backing: self.backing[m], low: v.supply, seq: self.report_seq, direct: false }
    }

    /// Shares change hands between a read and its report: a buy and a sale
    /// (the sale burns shares minted since the read), a buy and a direct
    /// burn of some of what it minted, or a sale (the supply dips below the
    /// read) and then a buy.
    fn trade_since_read(&mut self, m: usize) {
        let a = self.rng.below(ACTORS as u64) as usize;
        match self.rng.below(4) {
            0 => {
                // Often only shares this buy minted, so the sale's burns are
                // all of shares minted since the read.
                let before = self.env.balance(&self.ptoken(a, m));
                self.mint_of(a, m);
                let got = self.env.balance(&self.ptoken(a, m)).saturating_sub(before);
                let cap = if self.rng.chance(60) { got } else { u64::MAX };
                self.redeem_up_to(a, m, cap);
            }
            1 => {
                let before = self.env.balance(&self.ptoken(a, m));
                self.mint_of(a, m);
                let got = self.env.balance(&self.ptoken(a, m)).saturating_sub(before);
                if got > 0 {
                    let n = 1 + self.rng.below(got);
                    self.burn_direct_of(a, m, n);
                    self.left_supply();
                    if let Some(r) = self.reads[m].as_mut() {
                        r.direct = true;
                    }
                }
            }
            2 => {
                self.redeem_of(a, m);
                self.mint_of(a, m);
            }
            // The same dip, burned straight through SPL Token.
            _ => {
                let bal = self.env.balance(&self.ptoken(a, m));
                if bal > 0 {
                    let n = 1 + self.rng.below(bal);
                    self.burn_direct_of(a, m, n);
                    self.left_supply();
                    if let Some(r) = self.reads[m].as_mut() {
                        r.direct = true;
                    }
                }
                self.mint_of(a, m);
            }
        }
        self.left_supply();
    }

    /// An actor whose USDC is not frozen, if any.
    fn unfrozen(&mut self) -> usize {
        let a0 = self.rng.below(ACTORS as u64) as usize;
        (0..ACTORS).map(|i| (a0 + i) % ACTORS).find(|&i| !self.frozen[i]).unwrap_or(a0)
    }

    /// A read lands after a newer read's report, with a dip between the
    /// two reads that the newer report hedged back: the old read's bound
    /// is below what is backed now, and its report (nothing lost) must not
    /// lower backing.
    ///
    /// Or the old read itself is reported after the dip and recovery, then
    /// reported again after more trading: its first report is held to the
    /// dip, and so must its second be, though the supply since that report
    /// may never have come near it (the burns up to the report bound it).
    fn stale_after_dip(&mut self, m: usize) {
        let full = self.read_now(m);
        self.report_of(m, full, full.supply, false);
        self.left_supply();
        let mut old = self.read_now(m);
        let a = self.unfrozen();
        // The dip: a sale (which may wait as a delayed sale, no dip), or a
        // direct burn by whoever holds the most.
        if self.rng.chance(50) {
            self.redeem_of(a, m);
        } else {
            let h = (0..ACTORS).max_by_key(|&h| self.env.balance(&self.ptoken(h, m))).unwrap();
            let bal = self.env.balance(&self.ptoken(h, m));
            if bal > 0 {
                let n = 1 + self.rng.below(bal);
                self.burn_direct_of(h, m, n);
            }
        }
        self.left_supply();
        old.low = old.low.min(self.supplies[m]);
        let dipped = old.low < old.supply;
        self.in_band = true;
        let usdc_in = 1 + self.rng.below(200 * USDC);
        self.mint_usdc(a, m, usdc_in);
        self.in_band = false;
        self.left_supply();
        if self.rng.chance(50) {
            let amount = old.supply + self.rng.below(2);
            self.report_of(m, old, amount, false);
            self.left_supply();
            if self.rng.chance(50) {
                self.in_band = true;
                let usdc_in = 1 + self.rng.below(100 * USDC);
                self.mint_usdc(a, m, usdc_in);
                self.in_band = false;
                self.left_supply();
            }
            old.low = old.low.min(self.supplies[m]);
            let was = self.backing[m];
            self.report_of(m, old, amount, false);
            if dipped && self.env.supply(&self.markets[m].mint) > old.low && was < old.supply {
                self.hit("a stale read reported again after a dip it was held to");
            }
            return;
        }
        let newer = self.read_now(m);
        self.report_of(m, newer, newer.supply, false);
        self.left_supply();
        old.low = old.low.min(self.supplies[m]);
        self.report_of(m, old, old.supply, false);
    }

    /// The desk reads its holdings now (counts from the view) and reports
    /// them now or later, fully hedged, partly, not at all, just what was
    /// backed then (no more hedged since, nothing lost) or a little less;
    /// now and then from an older read, which may predate the last report.
    fn report(&mut self) {
        if self.rng.chance(12) {
            self.skipped_reports();
            return;
        }
        let m = self.rng.below(MARKETS as u64) as usize;
        if self.rng.chance(15) {
            self.stale_after_dip(m);
            return;
        }
        let old = self.old_reads[m].is_some() && self.rng.chance(30);
        let read = if old {
            self.old_reads[m].take().unwrap()
        } else {
            if self.reads[m].is_none() || self.rng.chance(50) {
                // The read this one replaces may still land, late.
                if let Some(r) = self.reads[m].take() {
                    if self.rng.chance(40) {
                        self.old_reads[m] = Some(r);
                    }
                }
                self.reads[m] = Some(self.read_now(m));
                if self.rng.chance(50) {
                    return; // reported on a later step: burns in between count
                }
                if self.rng.chance(50) {
                    // Often from a market just hedged in full, where the risk
                    // left is exactly what the trades since the read add, so
                    // a report that mistreats them shows on the floor.
                    if self.rng.chance(40) {
                        let full = self.read_now(m);
                        self.report_of(m, full, full.supply, false);
                        self.left_supply();
                        self.reads[m] = Some(self.read_now(m));
                    }
                    self.trade_since_read(m);
                }
            }
            self.reads[m].take().unwrap()
        };
        // After burns since the read, often just what was backed then (where
        // counting those burns as lost backing would show), or a little less
        // (where discounting lost backing by them would).
        let since = self.burns[m] > read.burns;
        let pick = match self.rng.below(100) {
            0..=39 if since => 3,
            40..=59 if since => 4,
            _ => self.rng.below(8),
        };
        let (supply, held) = (read.supply, read.backing);
        let amount = match pick {
            0 => supply,
            1 => supply / 2,
            2 => 0,
            3 | 5 => held,
            // A little lost: the market is marked unknown, and if it was
            // nearly all hedged (the next case) it has room left to mint.
            4 => held.saturating_sub(1 + self.rng.below(20 * USDC)),
            6 => supply.saturating_sub(self.rng.below(20 * USDC)),
            _ => supply + self.rng.below(10 * USDC),
        };
        self.report_of(m, read, amount, true);
    }

    /// Report `amount` as read in `read`, and check what the report did
    /// against the model.
    fn report_of(&mut self, m: usize, read: Read, amount: u64, again: bool) {
        let mm = Env::market_keys(self.markets[m].id);
        let over = self.backed_over_supply(m);
        let pre = self.env.market(&mm);
        // The report books direct burns first, as the view does.
        let pre_v = self.env.view_market(&mm);
        let r = self.env.report(&mm, amount, read.seen, read.minted);
        assert!(self.outcome("report", &r, &[]));
        let mk = self.env.market(&mm);
        let supply = self.env.supply(&mm.mint);
        assert!(mk.backed <= supply, "report left backing above supply");
        // Skipped only for a read ahead of every burn, which the view never gives.
        assert!(read.seen <= mk.burned_total, "a report read from the view was skipped");
        if over {
            self.hit("an instruction while backed > supply");
        }
        let was = self.backing[m];
        assert_eq!(pre_v.backed, was, "market {m}: backing before the report not the model's");
        // Backing the desk held below what was backed, as it read it, is lost:
        // on an open market each such share is counted at $1 at once, and
        // the market is marked unknown; a paused one only marks it.
        let lost = was.saturating_sub(amount);
        let gone = self.burns[m] - read.burns;
        // The spec's lowest supply since the read: every burn since it before
        // any mint, or the lowest since the last report, held for a read
        // before that report to what the burns up to it could reach.
        let least = read.minted.saturating_sub(self.burns[m]).max(self.low[m].min(read.minted.saturating_sub(self.burned_at_report[m])));
        let now_backed = if lost == 0 { amount.min(least.max(was)) } else { amount.min(least) }.min(supply);
        // Independently of that: the desk sells only down to the supply, so
        // it holds at least what it read or the lowest supply since, and
        // (nothing lost) still what was backed. A report never credits more,
        // and nothing lost, never lowers backing.
        assert!(
            mk.backed <= amount.min(supply).min(read.low.max(was)),
            "market {m}: a report credited shares the desk may have sold since its read (backed {}, read {amount}, lowest supply since {}, was {was}) at step {}",
            mk.backed,
            read.low,
            self.steps
        );
        assert!(lost > 0 || mk.backed >= was, "market {m}: a report that lost nothing lowered backing ({was} -> {}) at step {}", mk.backed, self.steps);
        let live = !settled(&mk, self.env.now);
        let carried = |r: u128, held: u64| -> u128 {
            // The spec's `_bounded`: no more than the unbacked shares at `low` carry.
            if supply <= held { 0 } else { r.min((supply - held) as u128 * (DOLLAR - pre.low as u128)) }
        };
        if live && lost > 0 {
            let want = if mk.paused { pre_v.risk } else { pre_v.risk + lost as u128 * DOLLAR };
            assert_eq!(mk.risk, want, "market {m}: {lost} shares of backing lost, risk not raised by $1 each at step {}", self.steps);
            assert_eq!((mk.low, mk.high), (0, ONE), "market {m}: lost backing not marked unknown");
            self.unknown[m] = true;
            self.hit("backing lost on a report");
            if gone > 0 {
                self.hit("backing lost on a report with burns since its read");
            }
        } else if live {
            // Nothing lost, and the market's prices stand. Each share the
            // report hedges takes (1 - high) off, within the bound; otherwise
            // the risk stands (backing never falls here).
            let per = DOLLAR - pre.high.min(ONE) as u128;
            let want = if now_backed > was { carried(pre_v.risk.saturating_sub((now_backed - was) as u128 * per), now_backed) } else { pre_v.risk };
            assert_eq!(mk.risk, want, "market {m}: a report that lost no backing (backing {was} -> {now_backed}, {gone} burned since its read) left the wrong risk at step {}", self.steps);
            assert_eq!((mk.low, mk.high), (pre.low, pre.high), "market {m}: a report that lost no backing moved the prices at step {}", self.steps);
            if gone > 0 && mk.minted_total > read.minted {
                self.hit("a report after shares minted and burned since its read");
            }
            if read.direct {
                self.hit("a report after a direct burn of shares minted since its read");
            }
            if least.max(was) < amount.min(supply) {
                self.hit("a report held to the lowest supply since its read");
            }
            if now_backed > was && gone > 0 {
                self.hit("a hedge report with burns since its read");
            }
        }
        if read.seq < self.reported_at[m] {
            self.hit("a report from a read taken before the last report");
            if lost == 0 && least < was {
                self.hit("a read from before the last report bounded below what was backed");
            }
        }
        // The rest is unhedged, lost backing included unless paused, when it
        // is not counted yet.
        self.backing[m] = now_backed;
        self.fresh[m] = 0;
        self.low[m] = supply;
        self.burned_at_report[m] = self.burns[m];
        self.report_seq += 1;
        self.reported_at[m] = self.report_seq;
        let lost = if live && !mk.paused { lost } else { 0 };
        self.unbacked[m] = (self.unbacked[m] + lost).min((supply - amount.min(supply)).saturating_sub(self.riskless[m]));
        self.touched(m);
        if lost > 0 && self.rng.chance(70) {
            // After lost backing a buyer often comes next: its mint must count
            // the whole gap at $1. Small ones, so the gap at $1 leaves room
            // under the caps; and a second, as the market is still
            // unknown and must count the gap at $1 again.
            let a = self.unfrozen();
            for _ in 0..2 {
                self.left_supply();
                let usdc_in = 1 + self.rng.below(5 * USDC);
                self.in_band = true;
                self.mint_usdc(a, m, usdc_in);
                self.in_band = false;
            }
        } else if again && lost == 0 && gone > 0 && self.rng.chance(50) {
            // The desk still holds what it reported and says so again from a
            // fresh read. Had the report taken burns since its read off
            // backing without their risk, this one would take off, as a
            // hedge, risk the shares left unhedged carry: the floor shows it.
            self.left_supply();
            let fresh = self.read_now(m);
            self.hit("the same holdings reported again from a fresh read");
            self.report_of(m, fresh, amount, false);
        }
    }

    fn refresh(&mut self) {
        let ms: Vec<Mkt> = self.markets.iter().map(|m| Env::market_keys(m.id)).collect();
        let refs: Vec<&Mkt> = ms.iter().collect();
        let before = self.env.config().total_risk;
        let over: Vec<bool> = (0..MARKETS).map(|m| self.backed_over_supply(m)).collect();
        let r = self.env.refresh_risk(&refs);
        assert!(self.outcome("refresh_risk", &r, &[]));
        assert!(self.env.config().total_risk <= before, "refresh_risk raised the risk");
        for (m, over) in over.into_iter().enumerate() {
            if over {
                self.hit("an instruction while backed > supply");
            }
            self.touched(m);
        }
    }

    fn settle(&mut self) {
        let m = self.rng.below(MARKETS as u64) as usize;
        let mm = Env::market_keys(self.markets[m].id);
        let payout = match self.rng.below(3) {
            0 => 0,
            1 => ONE,
            _ => self.rng.below(ONE + 1),
        };
        let k = Env::as_kp(&self.env.keeper);
        let mk = self.env.market(&mm);
        let mut allowed = vec![];
        // The owner cancelled this keeper's settlement here before.
        if mk.barred == k.pubkey() {
            allowed.push(ex(ExchangeError::OnlyKeeper));
        }
        if mk.settle_at != 0 {
            allowed.push(ex(ExchangeError::AlreadySettled));
        }
        let r = self.env.settle(&k, &mm, payout);
        if self.outcome("settle", &r, &allowed) && payout == 0 {
            self.hit("a $0 settlement");
        }
    }

    fn cancel_settle(&mut self) {
        let m = self.rng.below(MARKETS as u64) as usize;
        let mm = Env::market_keys(self.markets[m].id);
        let o = Env::as_kp(&self.env.owner);
        let mk = self.env.market(&mm);
        let pending = mk.settle_at != 0 && self.env.now < mk.settle_at;
        let allowed = if pending { vec![] } else { vec![ex(ExchangeError::NothingPending)] };
        let r = self.env.cancel_settle(&o, &mm);
        self.outcome("cancel_settle", &r, &allowed);
    }

    fn pause(&mut self) {
        let m = self.rng.below(MARKETS as u64) as usize;
        let mm = Env::market_keys(self.markets[m].id);
        let k = Env::as_kp(&self.env.keeper);
        let paused = self.rng.chance(30);
        let r = self.env.set_paused(&k, &[&mm], paused);
        assert!(self.outcome("set_paused", &r, &[]));
    }

    fn halt_or_resume(&mut self) {
        let r = if self.env.config().halted {
            self.env.owner_ix(px::instruction::Resume {})
        } else {
            let k = Env::as_kp(&self.env.keeper);
            self.env.halt_by(&k)
        };
        assert!(self.outcome("halt_resume", &r, &[]));
    }

    fn bridge(&mut self) {
        let vault = self.env.balance(&self.env.vault.clone());
        let c = self.env.config();
        let free = c.free_float(vault);
        let k = Env::as_kp(&self.env.keeper);
        let to = self.env.bridge;
        // Never a unit of what the queue is owed.
        if c.queued > 0 || self.rng.chance(20) {
            let r = self.env.send_to_bridge_by(&k, to, free + 1);
            expect_err(r, ExchangeError::InsufficientFloat);
        }
        let amount = match self.rng.below(3) {
            0 => free,
            _ => self.rng.below(free + 1),
        };
        if amount == 0 {
            return;
        }
        let r = self.env.send_to_bridge_by(&k, to, amount);
        assert!(self.outcome("send_to_bridge", &r, &[]));
        self.bridged += amount;
        assert!(self.env.balance(&self.env.vault.clone()) >= self.env.config().queued.min(vault), "the bridge took owed USDC");
    }

    fn fund(&mut self) {
        let amount = self.rng.below(3_000 * USDC);
        if amount > 0 {
            self.env.fund_vault(amount);
            self.funded += amount;
        }
    }

    fn pay_queue(&mut self) {
        let max = 1 + self.rng.below(10) as u8;
        let allowed = if self.env.config().halted { vec![ex(ExchangeError::Halted)] } else { vec![] };
        let r = self.env.pay_queue(max);
        self.outcome("pay_queue", &r, &allowed);
    }

    /// A mint, sale or release raced by another transaction paying the head
    /// (see `queue_list`). The random mix seldom leaves two claims in line,
    /// so this first makes some when there are fewer: the keeper bridges the
    /// free float, then actors sell into the empty float.
    fn queue_race(&mut self) {
        let deep = |c: &Config| c.claim_head + 1 < c.claim_tail;
        if !deep(&self.env.config()) {
            let c = self.env.config();
            let free = c.free_float(self.env.balance(&self.env.vault.clone()));
            if free > 0 {
                let (k, to) = (Env::as_kp(&self.env.keeper), self.env.bridge);
                let r = self.env.send_to_bridge_by(&k, to, free);
                assert!(self.outcome("send_to_bridge", &r, &[]));
                self.bridged += free;
            }
            for _ in 0..3 {
                self.redeem();
                self.left_supply();
            }
        }
        self.force_race = true;
        match self.rng.below(3) {
            0 => self.mint(),
            1 => self.redeem(),
            _ => self.release(),
        }
        self.force_race = false;
    }

    fn live_tickets(&self) -> Vec<(u64, DelayedSale)> {
        (0..self.env.config().delayed_next).filter_map(|i| self.env.ticket_at(i).map(|t| (i, t))).collect()
    }

    fn release(&mut self) {
        let live = self.live_tickets();
        if live.is_empty() {
            return;
        }
        let (i, t) = live[self.rng.below(live.len() as u64) as usize].clone();
        let m = self.markets.iter().position(|m| m.id == t.position_id).unwrap();
        let over = self.backed_over_supply(m);
        // Races more often than a mint or sale: releases are rarer, and the
        // head rule must be seen to hold for them too.
        let c = self.env.config();
        let empty = c.claim_head < c.claim_tail && self.rng.chance(15);
        let lacks_head = if empty {
            // An empty list on a live queue: the program's head rule alone refuses it.
            self.headless = None;
            *self.env.queue_list.borrow_mut() = Some(vec![]);
            true
        } else {
            self.queue_list(60)
        };
        let mut allowed = vec![];
        if self.env.config().halted {
            allowed.push(ex(ExchangeError::Halted));
        }
        if self.env.now < t.ready_at {
            allowed.push(ex(ExchangeError::NotYet));
        }
        if lacks_head {
            allowed.push(ex(ExchangeError::MissingAccount));
        }
        let r = self.env.release(i);
        if lacks_head && matches!(r, Err(TransactionError::InstructionError(_, InstructionError::Custom(c))) if c == ex(ExchangeError::MissingAccount)) {
            self.hit(if empty { "a release refused for an empty list" } else { "a release refused for a list without the head" });
        }
        if self.outcome("release", &r, &allowed) {
            // A release always pays, so it must offer a live queue's head.
            assert!(!lacks_head, "a release went through without the queue head");
            if over {
                self.hit("an instruction while backed > supply");
            }
            self.touched(m);
        }
    }

    fn cancel(&mut self) {
        let live = self.live_tickets();
        if live.is_empty() {
            return;
        }
        let (i, t) = live[self.rng.below(live.len() as u64) as usize].clone();
        let a = self.actor_of(&t.to).unwrap();
        let m = self.markets.iter().position(|m| m.id == t.position_id).unwrap();
        let dest = self.ptoken(a, m);
        let o = Env::as_kp(&self.env.owner);
        let before = self.env.balance(&dest);
        let r = self.env.cancel_by(&o, i, dest);
        assert!(self.outcome("cancel", &r, &[]));
        self.promised[a] -= t.amount;
        assert_eq!(self.env.balance(&dest) - before, t.p_amount, "cancel returned the wrong shares");
    }

    fn freeze(&mut self) {
        let a = self.rng.below(ACTORS as u64) as usize;
        let usdc = self.actors[a].usdc;
        self.frozen[a] = !self.frozen[a];
        self.env.freeze(&usdc, self.frozen[a]);
    }

    fn withdraw(&mut self) {
        let asides = self.set_aside_claims();
        if asides.is_empty() {
            return;
        }
        let (addr, cl) = asides[self.rng.below(asides.len() as u64) as usize].clone();
        // Set aside because its account was frozen; usually thawed by now.
        let a = self.actor_of(&cl.to).unwrap();
        if self.frozen[a] && self.rng.chance(70) {
            self.frozen[a] = false;
            self.env.freeze(&cl.to, false);
        }
        let allowed = if self.frozen[a] { vec![FROZEN] } else { vec![] };
        let r = self.env.withdraw_unclaimed(addr);
        self.outcome("withdraw_unclaimed", &r, &allowed);
    }

    fn warp(&mut self) {
        let secs = match self.rng.below(3) {
            0 => 1 + self.rng.below(60),
            1 => 1 + self.rng.below(HOUR as u64),
            _ => 1 + self.rng.below(2 * HOUR as u64),
        };
        self.env.warp(secs as i64);
    }

    fn step(&mut self) {
        self.steps += 1;
        // A client reads a live queue now and sends on a later step.
        if self.env.config().claim_head < self.env.config().claim_tail && self.rng.chance(30) {
            self.old_queue = Some(self.env.queue_metas(QUEUE_BATCH));
        }
        // A halt is short-lived (the owner resumes), so trading gets a chance.
        if self.env.config().halted && self.rng.chance(40) {
            self.halt_or_resume();
        }
        let c = self.env.config();
        match self.rng.below(100) {
            0..=22 => self.mint(),
            23..=40 => self.redeem(),
            41..=46 => self.burn_direct(),
            47..=50 => self.absorb(),
            51..=58 => self.report(),
            59..=61 => self.refresh(),
            62 => self.settle(),
            63..=64 => self.cancel_settle(),
            65..=66 => self.pause(),
            67 => self.halt_or_resume(),
            68..=72 => self.bridge(),
            73..=76 => self.fund(),
            77..=80 => self.pay_queue(),
            81..=84 => self.release(),
            85 => self.cancel(),
            86..=87 => self.freeze(),
            88..=89 => self.withdraw(),
            90..=92 => self.queue_race(),
            _ => self.warp(),
        }
        // A list left unused (the action returned early) is not kept for the next one.
        self.env.queue_list.borrow_mut().take();
        self.headless = None;
        let after = self.env.config();
        if after.claim_tail > c.claim_tail {
            self.hit("a claim queued");
        }
        if after.aside_next > c.aside_next {
            self.hit("a release payment set aside");
        }
        if (c.claim_head..after.claim_head).any(|i| self.env.claim_at(&pda(&[CLAIM_SEED, &i.to_le_bytes()])).is_some_and(|cl| cl.set_aside)) {
            self.hit("a queued claim set aside");
        }
        self.left_supply();
        self.check();
    }

    /* --------------------------------------------------------- invariants */

    /// Shares that left a market's supply since the last step, burned by the
    /// exchange or straight through SPL Token, come off the model's unhedged
    /// count: it assumes the most risk-carrying shares went first.
    fn left_supply(&mut self) {
        for m in 0..MARKETS {
            self.left_supply_of(m);
        }
    }

    /// The same for one market. Shares that left also come off the model's
    /// backing, except up to `fresh` of them, which leave the unbacked gap.
    fn left_supply_of(&mut self, m: usize) {
        let supply = self.env.supply(&self.markets[m].mint);
        let was = self.supplies[m];
        if supply < was {
            let n = was - supply;
            self.unbacked[m] = self.unbacked[m].saturating_sub(n);
            self.burns[m] += n;
            let k = self.fresh[m].min(n);
            self.fresh[m] -= k;
            self.backing[m] = self.backing[m].saturating_sub(n - k).min(supply);
        }
        self.supplies[m] = supply;
        self.low[m] = self.low[m].min(supply);
        for r in [self.reads[m].as_mut(), self.old_reads[m].as_mut()].into_iter().flatten() {
            r.low = r.low.min(supply);
        }
    }

    /// A direct burn left backing above the supply the next instruction reads.
    fn backed_over_supply(&self, m: usize) -> bool {
        self.env.market(&self.markets[m]).backed > self.env.supply(&self.markets[m].mint)
    }

    /// After an instruction that burned, reported or refreshed market `m`, or
    /// minted at its final payout: its books are synced, and a settled
    /// market carries no risk (spec invariant 5).
    fn touched(&mut self, m: usize) {
        self.synced(m);
        let mk = self.env.market(&self.markets[m]);
        if settled(&mk, self.env.now) {
            assert_eq!(mk.risk, 0, "market {m}: settled but still carries risk after {} at step {}", self.last, self.steps);
        }
    }

    /// After an exchange instruction on market `m`, every burn, direct ones
    /// included, is on its books, and backing is exactly the model's: a
    /// burn beyond the quoted mints since the report lowered it.
    fn synced(&mut self, m: usize) {
        self.left_supply_of(m);
        let mk = self.env.market(&self.markets[m]);
        let supply = self.env.supply(&self.markets[m].mint);
        assert_eq!(mk.burned_total + supply, mk.minted_total, "market {m}: burns not all counted after {} at step {}", self.last, self.steps);
        assert!(mk.backed <= supply, "market {m}: backed above supply after an exchange instruction");
        assert_eq!(mk.backed, self.backing[m], "market {m}: backing not what the burns since the report leave, after {} at step {}", self.last, self.steps);
    }

    fn claims_in_line(&self) -> Vec<Claim> {
        let c = self.env.config();
        (c.claim_head..c.claim_tail)
            .map(|i| self.env.claim_at(&pda(&[CLAIM_SEED, &i.to_le_bytes()])).expect("claim in line is missing"))
            .collect()
    }

    /// Every set-aside claim: queue claims moved aside and release payments set aside.
    fn set_aside_claims(&self) -> Vec<(Pubkey, Claim)> {
        let c = self.env.config();
        let mut v = vec![];
        for i in 0..c.claim_head {
            let addr = pda(&[CLAIM_SEED, &i.to_le_bytes()]);
            if let Some(cl) = self.env.claim_at(&addr) {
                assert!(cl.set_aside, "a claim behind the head is neither paid nor set aside");
                v.push((addr, cl));
            }
        }
        for i in 0..c.aside_next {
            let addr = pda(&[ASIDE_SEED, &i.to_le_bytes()]);
            if let Some(cl) = self.env.claim_at(&addr) {
                v.push((addr, cl));
            }
        }
        v
    }

    fn received(&self, a: usize) -> u64 {
        // Never negative: an actor cannot have spent more than it had plus what it was paid.
        (self.env.balance(&self.actors[a].usdc) + self.spent[a]).checked_sub(START_USDC).expect("an actor lost USDC")
    }

    fn check(&self) {
        let env = &self.env;
        let c = env.config();
        let vault = env.balance(&env.vault.clone());

        // 1. The queue: queued = live claims + set-aside claims; the float holds the set-aside USDC.
        let line = self.claims_in_line();
        let asides = self.set_aside_claims();
        let live: u64 = line.iter().map(|cl| cl.amount).sum();
        let aside: u64 = asides.iter().map(|(_, cl)| cl.amount).sum();
        assert!(line.iter().all(|cl| !cl.set_aside), "a set-aside claim is still in line");
        assert_eq!(c.unclaimed_total, aside, "unclaimed_total != set-aside claims");
        assert_eq!(c.queued, live + aside, "queued != claims in line + set aside");
        assert!(vault >= c.unclaimed_total, "set-aside USDC not held");
        // Free float excludes everything owed (the bridge's bound). The view
        // is a simulated transaction, about half the run's time if read on
        // every step, so it is read on every eighth and at the end.
        if self.steps % 8 == 0 || self.unwinding {
            assert_eq!(env.view_float().free_float, vault.saturating_sub(c.queued));
        }

        // 2. Every actor was paid or is owed exactly what it was promised, never more.
        let tickets = self.live_tickets();
        let mut owed = vec![0u64; ACTORS];
        for cl in line.iter().chain(asides.iter().map(|(_, cl)| cl)) {
            owed[self.actor_of(&cl.to).expect("a claim to a stranger")] += cl.amount;
        }
        for (_, t) in &tickets {
            owed[self.actor_of(&t.to).expect("a ticket to a stranger")] += t.amount;
        }
        let mut paid = 0;
        for a in 0..ACTORS {
            let got = self.received(a);
            assert_eq!(got + owed[a], self.promised[a], "actor {a}: paid {got} + owed {} != promised {}", owed[a], self.promised[a]);
            paid += got;
        }

        // 3. The float moved only by mints, outside funding, the bridge and payments.
        let spent: u64 = self.spent.iter().sum();
        assert_eq!(vault + self.bridged + paid, self.funded + spent, "float out of balance");

        // 4. total_risk is the sum of the markets' risk.
        let mut sum = 0u128;
        for (m, mm) in self.markets.iter().enumerate() {
            let mk = env.market(mm);
            sum += mk.risk;
            // 5. Delayed shares are exactly the live tickets' and sit in held.
            let p: u64 = tickets.iter().filter(|(_, t)| t.position_id == mm.id).map(|(_, t)| t.p_amount).sum();
            assert_eq!(mk.delayed_shares, p, "market {m}: delayed_shares != live tickets");
            assert!(env.balance(&mm.held) >= mk.delayed_shares, "market {m}: held short of delayed shares");
            // 6. Supply bookkeeping: what the exchange minted, less what it
            //    has seen burned, is at least the supply (direct burns not yet
            //    seen make up the rest), and backing never exceeds it.
            let supply = env.supply(&mm.mint);
            assert!(mk.burned_total + supply <= mk.minted_total, "market {m}: more supply than minted");
            assert!(mk.backed <= mk.minted_total - mk.burned_total, "market {m}: backed above the supply as last seen");
            assert!(mk.minted_since_report <= mk.minted_total);
            // 7. Risk is never understated: until the market settles it holds
            //    at least what the model's unhedged quoted shares carry.
            if !settled(&mk, env.now) {
                assert_eq!(mk.high == ONE, self.unknown[m], "market {m}: marked unknown ({}) unlike the model ({}) after {} at step {}", mk.high == ONE, self.unknown[m], self.last, self.steps);
                let least = self.unbacked[m] as u128 * (DOLLAR - self.hi[m] as u128);
                assert!(
                    mk.risk >= least,
                    "market {m}: risk {} below the least it must carry, {least} ({} unhedged shares minted at up to {}), after {} at step {}",
                    mk.risk,
                    self.unbacked[m],
                    self.hi[m],
                    self.last,
                    self.steps
                );
            }
        }
        assert_eq!(c.total_risk, sum, "total_risk != sum of markets");
    }

    /* ------------------------------------------------------------- unwind */

    /// After resume, unfreeze and time, everyone gets out: every ticket
    /// released, the queue paid, set-aside claims withdrawn, every share
    /// redeemed, and each actor holds exactly what it was promised.
    fn unwind(&mut self) {
        self.unwinding = true;
        if self.env.config().halted {
            self.env.owner_ix(px::instruction::Resume {}).unwrap();
        }
        self.env.owner_ix(px::instruction::SetOutflowCap { floor: u64::MAX / 4, float_bps: 0 }).unwrap();
        for a in 0..ACTORS {
            if self.frozen[a] {
                let usdc = self.actors[a].usdc;
                self.env.freeze(&usdc, false);
                self.frozen[a] = false;
            }
        }
        self.env.warp(2 * HOUR);
        self.env.fund_vault(10_000_000 * USDC);
        self.funded += 10_000_000 * USDC;
        for (i, _) in self.live_tickets() {
            self.env.release(i).unwrap();
        }
        for _ in 0..200 {
            let c = self.env.config();
            if c.claim_head == c.claim_tail {
                break;
            }
            self.env.pay_queue(10).unwrap();
        }
        let c = self.env.config();
        assert_eq!(c.claim_head, c.claim_tail, "LIVENESS: the queue cannot be paid");
        for (addr, _) in self.set_aside_claims() {
            self.env.withdraw_unclaimed(addr).unwrap();
        }
        assert_eq!(self.env.config().queued, 0, "LIVENESS: queued left");
        self.left_supply();
        self.check();
        // Everyone redeems everything.
        for m in 0..MARKETS {
            for a in 0..ACTORS {
                let bal = self.env.balance(&self.ptoken(a, m));
                if bal == 0 {
                    continue;
                }
                let mk = self.env.market(&self.markets[m]);
                let u = User { kp: Env::as_kp(&self.actors[a].kp), usdc: self.actors[a].usdc };
                let settled = settled(&mk, self.env.now);
                let (price, fee) = if settled { (mk.payout, SETTLE_FEE) } else { (500_000, SELL_SPREAD) };
                let r = if settled {
                    self.env.redeem_at_payout(&u, &self.markets[m], bal)
                } else {
                    self.env.sell(&u, &self.markets[m], bal, price)
                };
                r.unwrap_or_else(|e| panic!("LIVENESS: actor {a} cannot redeem market {m}: {e:?}"));
                self.promised[a] += model_redeem_out(price, bal, fee);
                self.touched(m);
            }
        }
        assert_eq!(self.env.config().queued, 0, "LIVENESS: final redemptions queued");
        assert!(self.live_tickets().is_empty(), "LIVENESS: final redemptions delayed");
        self.left_supply();
        self.check();
        for a in 0..ACTORS {
            assert_eq!(self.received(a), self.promised[a], "actor {a} not paid exactly what it was promised");
        }
    }
}

#[test]
fn random_actions_keep_every_invariant() {
    let first = env_u64("PX_FUZZ_SEED", 1);
    let runs = env_u64("PX_FUZZ_RUNS", 8);
    let steps = env_u64("PX_FUZZ_STEPS", 400);
    let mut totals: HashMap<&'static str, (u64, u64)> = HashMap::new();
    let mut refusals: HashMap<(&'static str, u32), u64> = HashMap::new();
    let mut hits: HashMap<&'static str, u64> = HashMap::new();
    for seed in first..first + runs {
        let mut w = World::new(seed);
        for _ in 0..steps {
            w.step();
        }
        w.unwind();
        for (k, (ok, err)) in w.counts {
            let t = totals.entry(k).or_default();
            t.0 += ok;
            t.1 += err;
        }
        for (k, n) in w.refusals {
            *refusals.entry(k).or_default() += n;
        }
        for (k, n) in w.hits {
            *hits.entry(k).or_default() += n;
        }
    }
    if std::env::var("PX_FUZZ_VERBOSE").is_ok() {
        let mut r: Vec<_> = refusals.iter().collect();
        r.sort();
        for ((name, c), n) in r {
            eprintln!("{name:>20} refused with {c}: {n}");
        }
    }
    let mut names: Vec<_> = totals.iter().collect();
    names.sort();
    for (name, (ok, err)) in &names {
        eprintln!("{name:>20}: {ok} ok, {err} refused");
    }
    let mut reached: Vec<_> = hits.iter().collect();
    reached.sort();
    for (what, n) in &reached {
        eprintln!("{what:>40}: {n}");
    }
    if runs * steps >= 1_000 {
        // Coverage: every kind of action went through at least once overall...
        for name in [
            "mint",
            "redeem",
            "burn_direct",
            "absorb",
            "report",
            "release",
            "pay_queue",
            "send_to_bridge",
            "settle",
            "cancel",
            "withdraw_unclaimed",
        ] {
            assert!(totals.get(name).is_some_and(|t| t.0 > 0), "{name} never succeeded");
        }
        // ...and the states the dangerous paths need were reached, so a change
        // to the action mix that stops reaching one fails here, not silently.
        for what in [
            "an instruction while backed > supply",
            "a sale delayed",
            "a claim queued",
            "a queued claim set aside",
            "a release payment set aside",
            "a redeem at the payout",
            "a $0 settlement",
            "a direct burn beyond the mints since the report",
            "backing lost on a report",
            "backing lost on a report with burns since its read",
            "a report after shares minted and burned since its read",
            "a report after a direct burn of shares minted since its read",
            "a report held to the lowest supply since its read",
            "a report from a read taken before the last report",
            "a read from before the last report bounded below what was backed",
            "the same holdings reported again from a fresh read",
            "a hedge report with burns since its read",
            "a skipped report with a direct burn pending",
            "a skipped report batch with one market twice and a burn pending",
            "a stale read reported again after a dip it was held to",
            "a direct burn booked on a market minted at more than one price",
            "a quoted mint on a market marked unknown",
            "a second quoted mint on a market marked unknown",
            "dust over a spent cap refused",
            "under $1 over the cap paid now",
            "exactly MIN_DELAYED over the cap ticketed",
        ] {
            assert!(hits.get(what).is_some_and(|n| *n > 0), "the fuzz never reached: {what}");
        }
        // Stale queue lists: claims paid and claims set aside since the read
        // were both skipped; lists holding claims but not the head, with only
        // closed accounts where the head was or with a set-aside claim, were
        // refused; and so was a release without the head.
        for what in [
            "stale list: a paid claim skipped",
            "stale list: a set-aside claim skipped",
            "a list of paid claims but not the head refused",
            "a list with a set-aside claim but not the head refused",
            "a release refused for a list without the head",
            "a release refused for an empty list",
        ] {
            assert!(hits.get(what).is_some_and(|n| *n > 0), "the fuzz never reached: {what}");
        }
    }
}
