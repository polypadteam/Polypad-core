//! contracts/test/fuzz/ForgedRiskLadder.t.sol on LiteSVM: a stolen signing
//! key minting at rising prices (5c, 50c, ..., 98c) cannot hold more unhedged
//! exposure than `maxRisk`, because each mint carries the risk it added and
//! the unbacked supply is valued at the lowest price minted since the market
//! was last fully backed. Run `anchor build` first.

mod common;

use common::*;

const ID_A: u8 = 1;

/// PolypadBase's parameters: band 5c-98c, $1,000 per market, no total cap.
fn setup() -> (Env, Mkt, User) {
    let mut env = Env::new();
    env.owner_ix(px::instruction::SetParams {
        buy_spread_bps: 25,
        sell_spread_bps: 25,
        min_price: 50_000,
        max_price: 980_000,
        default_max_risk: 1_000 * USDC,
    })
    .unwrap();
    env.owner_ix(px::instruction::SetMaxTotalRisk { cap: u64::MAX }).unwrap();
    let m = env.create_market(pid(ID_A));
    let attacker = env.user(1_000_000 * USDC);
    (env, m, attacker)
}

/// A mint at a quote the attacker signed with the stolen key (the test
/// signer); returns the shares minted.
fn forged_mint(env: &mut Env, who: &User, m: &Mkt, price: u64, usdc_in: u64) -> Result<u64, Res> {
    let before = env.supply(&m.mint);
    match env.buy(who, m, usdc_in, price) {
        Ok(()) => Ok(env.supply(&m.mint) - before),
        Err(e) => Err(Err(e)),
    }
}

/// Mint as many shares as the cap allows at `price`, valued at `low`.
fn forged_mint_max(env: &mut Env, who: &User, m: &Mkt, price: u64, low: u64) -> u64 {
    let room = (env.config().default_max_risk as u128 * ONE as u128 / (ONE - low) as u128) as u64 + env.market(m).backed;
    let want = room - env.supply(&m.mint);
    let paid = (want as u128 * price as u128 * 10_025 / 10_000_000_000) as u64;
    forged_mint(env, who, m, price, paid).unwrap()
}

fn risk(env: &Env, m: &Mkt) -> u128 {
    env.market(m).risk
}

fn report(env: &mut Env, m: &Mkt, amount: u64) {
    let v = env.view_market(m);
    env.report(m, amount, v.burned_total, v.minted_total).unwrap();
}

#[test]
fn ladder_cannot_exceed_max_risk() {
    let (mut env, m, attacker) = setup();
    let ladder = [50_000u64, 500_000, 800_000, 900_000, 950_000, 980_000];
    let n = forged_mint_max(&mut env, &attacker, &m, ladder[0], ladder[0]);
    let exposure = n as u128 * (ONE - ladder[0]) as u128 / ONE as u128;
    assert!(exposure <= env.config().default_max_risk as u128);
    assert_eq!(risk(&env, &m), n as u128 * 950_000);
    // Every higher rung is valued at 5c, so there is no room left: even $10 is refused.
    for &price in &ladder[1..] {
        expect_err(env.buy(&attacker, &m, 10 * USDC, price), ExchangeError::RiskCap);
    }
}

#[test]
fn two_rungs_refused() {
    let (mut env, m, attacker) = setup();
    forged_mint_max(&mut env, &attacker, &m, 50_000, 50_000);
    expect_err(env.buy(&attacker, &m, 1_000 * USDC, 980_000), ExchangeError::RiskCap);
}

/// A desk that trails one fill behind on a busy 98c market never pins it:
/// the unhedged gap is valued at what it could cost, not at every mint since.
#[test]
fn trailing_desk_does_not_pin_a_busy_market() {
    let (mut env, m, attacker) = setup();
    let mut last = 0;
    for _ in 0..80 {
        let supply = env.supply(&m.mint);
        report(&mut env, &m, supply - last); // hedged all but the latest fill
        env.warp(1);
        last = forged_mint(&mut env, &attacker, &m, 980_000, 1_000 * USDC).unwrap();
    }
    assert!(risk(&env, &m) < 50 * ONE as u128 * ONE as u128);
}

/// The desk's own allowance (a near-certain market left partly unhedged on
/// purpose) stays usable: exposure, not history, sets the room.
#[test]
fn unhedged_allowance_stays_usable() {
    let (mut env, m, attacker) = setup();
    let mut total = 0;
    for _ in 0..40 {
        env.warp(1);
        total += forged_mint(&mut env, &attacker, &m, 950_000, 950 * USDC).unwrap();
        // The desk keeps 10,000 shares unhedged ($500 at 95c).
        report(&mut env, &m, total.saturating_sub(10_000 * ONE));
    }
    // $500 left unhedged by policy, plus the latest mint until it is hedged.
    assert!(risk(&env, &m) <= 560 * ONE as u128 * ONE as u128);
}

/// A report that the desk hedged everything starts afresh at the next mint's price.
#[test]
fn report_resets_the_risk_price() {
    let (mut env, m, attacker) = setup();
    forged_mint_max(&mut env, &attacker, &m, 50_000, 50_000);
    let supply = env.supply(&m.mint);
    report(&mut env, &m, supply);
    let n = forged_mint(&mut env, &attacker, &m, 980_000, 10_000 * USDC).unwrap();
    assert!(n > 0);
    assert_eq!(risk(&env, &m), n as u128 * 20_000);
}

/// A partial report leaves the rest valued at the low price: the room left
/// at 98c is what the half still unhedged at 5c does not use, no more.
#[test]
fn partial_report_keeps_the_low_price() {
    let (mut env, m, attacker) = setup();
    let n = forged_mint_max(&mut env, &attacker, &m, 50_000, 50_000);
    report(&mut env, &m, n / 2);
    // The report brings the stored risk down to the half still unbacked at the low price at once.
    assert_eq!(risk(&env, &m), (n - n / 2) as u128 * 950_000);
    // ~$475 of room is left: 23,750 shares at 2c. $30k at 98c is too much...
    expect_err(env.buy(&attacker, &m, 30_000 * USDC, 980_000), ExchangeError::RiskCap);
    // ...$20k fits, and the total exposure stays within maxRisk.
    let more = forged_mint(&mut env, &attacker, &m, 980_000, 20_000 * USDC).unwrap();
    let exposure = (n - n / 2) as u128 * 950_000 / ONE as u128 + more as u128 * 20_000 / ONE as u128;
    assert!(exposure <= env.config().default_max_risk as u128 + 1);
}

/// The same ladder with the attacker burning shares straight through SPL
/// Token between rungs. Burned shares can never be redeemed, so they free
/// the room they used (as a redemption would), but no more: what is still
/// live stays within maxRisk.
#[test]
fn direct_burns_between_rungs_free_only_their_own_room() {
    let (mut env, m, attacker) = setup();
    let n = forged_mint_max(&mut env, &attacker, &m, 50_000, 50_000);
    let r = risk(&env, &m);
    env.burn_direct(&attacker, &m, n / 2).unwrap();
    // Nothing moves until an instruction looks.
    assert_eq!(risk(&env, &m), r);
    let live = n - n / 2;
    // The rest of the room, at 98c: ~$2,500 / 2c.
    let cap = env.config().default_max_risk as u128;
    let left = cap * ONE as u128 - live as u128 * 950_000;
    let shares = (left / 20_000) as u64;
    let too_much = (shares as u128 * 980_000 * 10_025 / 10_000_000_000) as u64 + 50 * USDC;
    expect_err(env.buy(&attacker, &m, too_much, 980_000), ExchangeError::RiskCap);
    let more = forged_mint(&mut env, &attacker, &m, 980_000, too_much - 100 * USDC).unwrap();
    let exposure = live as u128 * 950_000 / ONE as u128 + more as u128 * 20_000 / ONE as u128;
    assert!(exposure <= cap + 1);
    env.refresh_risk(&[&m]).unwrap();
    assert_eq!(env.config().total_risk, risk(&env, &m));
}
