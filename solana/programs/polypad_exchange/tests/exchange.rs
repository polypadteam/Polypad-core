//! `polypad_exchange` on LiteSVM: the Foundry suite's exchange and oracle
//! cases (contracts/test/Polypad.t.sol and attack/*.t.sol) that apply on
//! Solana, each against the built program with real token CPIs and a real
//! ed25519 precompile instruction. Run `anchor build` first.

mod common;

use anchor_lang::error::ErrorCode as AnchorError;
use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use common::*;
use ed25519_dalek::SigningKey;
use solana_keypair::Keypair;
use solana_signer::Signer;

const ID_A: u8 = 1;
const ID_B: u8 = 2;

fn anchor_code(e: AnchorError) -> u32 {
    e.into()
}

fn setup() -> (Env, Mkt) {
    let mut env = Env::new();
    let m = env.create_market(pid(ID_A));
    (env, m)
}

/* ------------------------------------------------------------ trading */

#[test]
fn quoted_mint_and_redeem_happy_path() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    // 600 USDC at 60c + 0.25%: the price rounds up, the shares down.
    let shares = shares_for(600 * USDC, 600_000);
    assert_eq!(shares, 600 * USDC * 1_000_000 / 601_500);
    let mine = env.ptoken_account(&alice, &m);
    assert_eq!(env.balance(&mine), shares);
    assert_eq!(env.supply(&m.mint), shares);
    assert_eq!(env.balance(&env.vault.clone()), 600 * USDC);
    assert_eq!(env.balance(&alice.usdc), 400 * USDC);
    let mk = env.market(&m);
    assert_eq!(mk.minted_since_report, shares);
    assert_eq!(mk.risk, shares as u128 * 400_000);
    assert_eq!(env.config().total_risk, mk.risk);

    // Back at 59c less the spread.
    env.sell(&alice, &m, shares, 590_000).unwrap();
    let out = usdc_for(shares, 590_000, 25);
    assert_eq!(env.balance(&alice.usdc), 400 * USDC + out);
    assert_eq!(env.balance(&mine), 0);
    assert_eq!(env.supply(&m.mint), 0);
    // A mint and sale back leaves no risk.
    assert_eq!((env.market(&m).risk, env.config().total_risk), (0, 0));
    assert!(out < 600 * USDC);
}

#[test]
fn compute_units_for_a_mint_and_a_redeem() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let q = env.quote(&m, BUY, 600_000, 100 * USDC);
    let to = env.ptoken_account(&alice, &m);
    let ixs = [env.ed25519_ix(&q), env.mint_ix(&alice.key(), &alice.usdc, &to, &m, 100 * USDC, 0, &q)];
    let cu_mint = env.send_meta(&ixs, &Env::as_kp(&alice.kp), &[]).unwrap();
    let q = env.quote(&m, SELL, 600_000, 50 * USDC);
    let ixs = [env.ed25519_ix(&q), env.redeem_ix(&alice, &m, 50 * USDC, 0, &q, &alice.usdc)];
    let cu_redeem = env.send_meta(&ixs, &Env::as_kp(&alice.kp), &[]).unwrap();
    println!("compute units: quoted mint {cu_mint}, quoted redeem {cu_redeem}");
    assert!(cu_mint < 200_000 && cu_redeem < 200_000);
}

#[test]
fn quote_must_be_verified_by_the_previous_instruction() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let q = env.quote(&m, BUY, 600_000, 100 * USDC);
    let to = env.ptoken_account(&alice, &m);
    let kp = Env::as_kp(&alice.kp);
    // No verification at all.
    let ix = env.mint_ix(&alice.key(), &alice.usdc, &to, &m, 100 * USDC, 0, &q);
    expect_err(env.send(std::slice::from_ref(&ix), &kp, &[]), ExchangeError::NotEd25519);
    // Signed by another key.
    let forged = Env::ed25519_ix_by(&SigningKey::from_bytes(&[7; 32]), &q.message(&px::ID));
    expect_err(env.send(&[forged, ix.clone()], &kp, &[]), ExchangeError::BadSignature);
    // Signed, then the price altered.
    let cheaper = Quote { price: 100_000, ..q };
    let ix2 = env.mint_ix(&alice.key(), &alice.usdc, &to, &m, 100 * USDC, 0, &cheaper);
    expect_err(env.send(&[env.ed25519_ix(&q), ix2], &kp, &[]), ExchangeError::BadSignature);
    // Verified, but not right before the mint.
    let between = anchor_lang::solana_program::system_instruction::transfer(&alice.key(), &alice.key(), 0);
    expect_err(env.send(&[env.ed25519_ix(&q), between, ix.clone()], &kp, &[]), ExchangeError::NotEd25519);
    // The honest pair works.
    env.send(&[env.ed25519_ix(&q), ix], &kp, &[]).unwrap();
}

#[test]
fn quote_fill_cap_holds_across_transactions_and_takers() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let bob = env.user(1_000 * USDC);
    let q = env.quote(&m, BUY, 600_000, 100 * USDC);
    env.buy_with(&alice, &m, 60 * USDC, &q).unwrap();
    // Replayed for more than is left: refused, by anyone.
    expect_err(env.buy_with(&alice, &m, 60 * USDC, &q), ExchangeError::QuoteTooSmall);
    expect_err(env.buy_with(&bob, &m, 41 * USDC, &q), ExchangeError::QuoteTooSmall);
    // The rest is usable (a quote binds no taker).
    env.buy_with(&bob, &m, 40 * USDC, &q).unwrap();
    expect_err(env.buy_with(&alice, &m, 1, &q), ExchangeError::QuoteTooSmall);
    // The same holds for sell quotes.
    let mine = env.ptoken_account(&alice, &m);
    let held = env.balance(&mine);
    let s = env.quote(&m, SELL, 590_000, held / 2);
    env.sell_with(&alice, &m, held / 2, &s).unwrap();
    expect_err(env.sell_with(&alice, &m, 2, &s), ExchangeError::QuoteTooSmall);
}

#[test]
fn expired_too_long_wrong_side_and_other_market_quotes_are_refused() {
    let (mut env, m) = setup();
    let other = env.create_market(pid(ID_B));
    let alice = env.user(1_000 * USDC);
    let q = env.quote(&m, BUY, 600_000, 100 * USDC);
    env.warp(16);
    expect_err(env.buy_with(&alice, &m, 10 * USDC, &q), ExchangeError::QuoteExpired);
    let long = Quote { valid_until: env.now + 31, ..env.quote(&m, BUY, 600_000, 100 * USDC) };
    expect_err(env.buy_with(&alice, &m, 10 * USDC, &long), ExchangeError::QuoteTooLong);
    let edge = Quote { valid_until: env.now + 30, ..long };
    env.buy_with(&alice, &m, 10 * USDC, &edge).unwrap();
    let sell = env.quote(&m, SELL, 600_000, 100 * USDC);
    expect_err(env.buy_with(&alice, &m, 10 * USDC, &sell), ExchangeError::WrongSide);
    let elsewhere = env.quote(&other, BUY, 600_000, 100 * USDC);
    expect_err(env.buy_with(&alice, &m, 10 * USDC, &elsewhere), ExchangeError::QuoteForOtherMarket);
    // No quote may price a share above 99.9c, on either side.
    let high = env.quote(&m, BUY, 999_001, 100 * USDC);
    expect_err(env.buy_with(&alice, &m, 10 * USDC, &high), ExchangeError::PriceOutOfRange);
    let mine = env.ptoken_account(&alice, &m);
    let held = env.balance(&mine);
    let high = env.quote(&m, SELL, 999_001, held);
    expect_err(env.sell_with(&alice, &m, held, &high), ExchangeError::PriceOutOfRange);
}

#[test]
fn band_and_slippage() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.owner_ix(px::instruction::SetParams {
        buy_spread_bps: 25,
        sell_spread_bps: 25,
        min_price: 10_000,
        max_price: 980_000,
        default_max_risk: 5_000 * USDC,
    })
    .unwrap();
    expect_err(env.buy(&alice, &m, 10 * USDC, 985_000), ExchangeError::PriceOutOfBand);
    expect_err(env.buy(&alice, &m, 10 * USDC, 9_999), ExchangeError::PriceOutOfBand);
    env.buy(&alice, &m, 10 * USDC, 980_000).unwrap();
    // minOut above what the price gives.
    let q = env.quote(&m, BUY, 500_000, 10 * USDC);
    let to = env.ptoken_account(&alice, &m);
    let ix = env.mint_ix(&alice.key(), &alice.usdc, &to, &m, 10 * USDC, shares_for(10 * USDC, 500_000) + 1, &q);
    expect_err(env.send(&[env.ed25519_ix(&q), ix], &Env::as_kp(&alice.kp), &[]), ExchangeError::Slippage);
}

#[test]
fn paused_market_refuses_buys_but_allows_sells() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 100 * USDC, 600_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    env.set_paused(&k, &[&m], true).unwrap();
    expect_err(env.buy(&alice, &m, 10 * USDC, 600_000), ExchangeError::MarketPaused);
    let mine = env.ptoken_account(&alice, &m);
    let held = env.balance(&mine);
    env.sell(&alice, &m, held, 600_000).unwrap();
    // Only the keeper pauses.
    let o = Env::as_kp(&env.owner);
    expect_err(env.set_paused(&o, &[&m], false), ExchangeError::OnlyKeeper);
    env.set_paused(&k, &[&m], false).unwrap();
    env.buy(&alice, &m, 10 * USDC, 600_000).unwrap();
}

/* --------------------------------------------------------- settlement */

#[test]
fn settlement_pays_payout_less_settle_fee_without_a_quote() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let shares = shares_for(600 * USDC, 600_000);
    let k = Env::as_kp(&env.keeper);
    env.settle(&k, &m, 1_000_000).unwrap();
    // Not yet in effect: no payout redemption, quoted sells still work.
    expect_err(env.redeem_at_payout(&alice, &m, shares), ExchangeError::NotAtPayout);
    env.warp(SETTLE_DELAY);
    // The float must cover $1 a share; the floor keeps it unmetered here.
    env.fund_vault(1_000 * USDC);
    let before = env.balance(&alice.usdc);
    env.redeem_at_payout(&alice, &m, shares / 2).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before, usdc_for(shares / 2, 1_000_000, 50));
    // A quoted redeem on a settled market ignores the quote: payout less fee.
    let junk = Quote { price: 1, ..env.quote(&m, SELL, 1, 0) };
    let rest = shares - shares / 2;
    let ix = env.redeem_ix(&alice, &m, rest, 0, &junk, &alice.usdc);
    env.send(&[ix], &Env::as_kp(&alice.kp), &[]).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before, usdc_for(shares / 2, 1_000_000, 50) + usdc_for(rest, 1_000_000, 50));
    // A settled market carries no risk.
    assert_eq!(env.market(&m).risk, 0);
    assert_eq!(env.config().total_risk, 0);
    // One-way: no second settlement, no cancel once in effect.
    expect_err(env.settle(&k, &m, 0), ExchangeError::AlreadySettled);
    let o = Env::as_kp(&env.owner);
    expect_err(env.cancel_settle(&o, &m), ExchangeError::NothingPending);
    // Quoted buys refuse a settled market's quote path... but a mint at the payout works.
    env.mint_at_payout(&alice, &m, 100 * USDC).unwrap();
}

#[test]
fn pending_dollar_payout_mints_at_one_dollar_at_once_keeping_risk() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let bob = env.user(1_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let risk = env.market(&m).risk;
    let k = Env::as_kp(&env.keeper);
    env.settle(&k, &m, 1_000_000).unwrap();
    let supply = env.supply(&m.mint);
    env.mint_at_payout(&bob, &m, 100 * USDC).unwrap();
    // $100 less the 0.25% spread buys about 99.75 shares at $1.
    assert_eq!(env.supply(&m.mint) - supply, shares_for(100 * USDC, 1_000_000));
    assert_eq!(shares_for(100 * USDC, 1_000_000), 99_750_623);
    // The stored risk stays in case the settlement is cancelled.
    assert_eq!(env.market(&m).risk, risk);
    // The quoted path prices at $1 too, ignoring its quote.
    let q = env.quote(&m, BUY, 600_000, 100 * USDC);
    env.buy_with(&bob, &m, 10 * USDC, &q).unwrap();
    assert_eq!(env.supply(&m.mint) - supply, shares_for(100 * USDC, 1_000_000) + shares_for(10 * USDC, 1_000_000));

    // Halted: refused. Cancelled: back to quoted buys only (and paused? no: it was open).
    let o = Env::as_kp(&env.owner);
    env.halt_by(&k).unwrap();
    expect_err(env.mint_at_payout(&bob, &m, 10 * USDC), ExchangeError::Halted);
    env.owner_ix(px::instruction::Resume {}).unwrap();
    env.cancel_settle(&o, &m).unwrap();
    expect_err(env.mint_at_payout(&bob, &m, 10 * USDC), ExchangeError::NotAtPayout);
    env.buy(&bob, &m, 10 * USDC, 600_000).unwrap();
}

#[test]
fn pending_partial_payout_waits_for_the_delay() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 100 * USDC, 600_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    env.settle(&k, &m, 500_000).unwrap();
    expect_err(env.mint_at_payout(&alice, &m, 10 * USDC), ExchangeError::NotAtPayout);
    expect_err(env.buy(&alice, &m, 10 * USDC, 600_000), ExchangeError::MarketPaused);
    env.warp(SETTLE_DELAY);
    env.mint_at_payout(&alice, &m, 10 * USDC).unwrap();
    // A settled market refuses a quote on the quoted path's check too.
    assert!(env.market(&m).settled(env.now));
}

#[test]
fn zero_payout_refuses_mints_and_redeems_for_nothing() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let bob = env.user(1_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    env.settle(&k, &m, 0).unwrap();
    env.warp(SETTLE_DELAY);
    expect_err(env.mint_at_payout(&bob, &m, 100 * USDC), ExchangeError::SettledAtZero);
    let q = env.quote(&m, BUY, 600_000, 100 * USDC);
    expect_err(env.buy_with(&bob, &m, 100 * USDC, &q), ExchangeError::SettledAtZero);
    let mine = env.ptoken_account(&alice, &m);
    let held = env.balance(&mine);
    let before = env.balance(&alice.usdc);
    env.redeem_at_payout(&alice, &m, held).unwrap();
    assert_eq!(env.balance(&alice.usdc), before);
    assert_eq!(env.supply(&m.mint), 0);
}

#[test]
fn cancel_settle_restores_the_pause_and_bars_that_keeper() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let k = Env::as_kp(&env.keeper);
    let o = Env::as_kp(&env.owner);
    // Settled while open: cancel reopens it.
    env.settle(&k, &m, 1_000_000).unwrap();
    assert!(env.market(&m).paused);
    // A recorded settlement cannot be unpaused by the keeper.
    env.set_paused(&k, &[&m], false).unwrap();
    assert!(env.market(&m).paused);
    // Only the owner cancels.
    expect_err(env.cancel_settle(&k, &m), ExchangeError::OnlyOwner);
    env.cancel_settle(&o, &m).unwrap();
    let mk = env.market(&m);
    assert!(!mk.paused);
    assert_eq!((mk.settle_at, mk.payout, mk.barred), (0, 0, k.pubkey()));
    env.buy(&alice, &m, 10 * USDC, 600_000).unwrap();
    // That keeper may not settle here again; the owner may.
    expect_err(env.settle(&k, &m, 1_000_000), ExchangeError::OnlyKeeper);
    env.settle(&o, &m, 0).unwrap();
    // The owner cancelling its own settlement bars nobody new.
    env.cancel_settle(&o, &m).unwrap();
    assert_eq!(env.market(&m).barred, k.pubkey());
    // A new keeper may settle.
    let k2 = Keypair::new();
    env.svm.airdrop(&k2.pubkey(), 1_000_000_000).unwrap();
    let bridge = env.bridge;
    env.owner_ix(px::instruction::SetRoles { keeper: k2.pubkey(), factory: Pubkey::default(), bridge_deposit: bridge }).unwrap();
    // Paused for an end date before settling: a cancel keeps it paused.
    env.set_paused(&k2, &[&m], true).unwrap();
    env.settle(&k2, &m, 1_000_000).unwrap();
    env.cancel_settle(&o, &m).unwrap();
    assert!(env.market(&m).paused);
    // Not after the delay.
    env.set_paused(&k2, &[&m], false).unwrap();
    let k3 = Keypair::new();
    env.svm.airdrop(&k3.pubkey(), 1_000_000_000).unwrap();
    env.owner_ix(px::instruction::SetRoles { keeper: k3.pubkey(), factory: Pubkey::default(), bridge_deposit: bridge }).unwrap();
    env.settle(&k3, &m, 1_000_000).unwrap();
    env.warp(SETTLE_DELAY);
    expect_err(env.cancel_settle(&o, &m), ExchangeError::NothingPending);
    // Payouts above $1 never.
    let m2 = env.create_market(pid(ID_B));
    expect_err(env.settle(&k3, &m2, 1_000_001), ExchangeError::PriceOutOfRange);
}

#[test]
fn halt_stops_trading_and_only_the_owner_resumes() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 100 * USDC, 600_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let stranger = Keypair::new();
    env.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    expect_err(env.halt_by(&stranger), ExchangeError::OnlyKeeper);
    env.halt_by(&k).unwrap();
    expect_err(env.buy(&alice, &m, 10 * USDC, 600_000), ExchangeError::Halted);
    expect_err(env.sell(&alice, &m, 1_000_000, 600_000), ExchangeError::Halted);
    expect_err(env.pay_queue(3), ExchangeError::Halted);
    // The bridge still works, so the desk can be funded.
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, USDC).unwrap();
    // The keeper cannot resume.
    let ix = env.admin_ix(k.pubkey(), px::instruction::Resume {});
    expect_err(env.send(&[ix], &k, &[]), ExchangeError::OnlyOwner);
    env.owner_ix(px::instruction::Resume {}).unwrap();
    env.buy(&alice, &m, 10 * USDC, 600_000).unwrap();
}

/* --------------------------------------------------------------- risk */

#[test]
fn risk_cap_stops_mints_until_hedged() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    // At 60c each unbacked share risks 40c: a $200 cap is 500 shares.
    let o = Env::as_kp(&env.owner);
    env.market_admin(&o, &m, px::instruction::SetMaxRisk { cap: 200 * USDC }).unwrap();
    env.buy(&alice, &m, 290 * USDC, 600_000).unwrap(); // ~482 shares, ~$193
    expect_err(env.buy(&alice, &m, 100 * USDC, 600_000), ExchangeError::RiskCap);
    let supply = env.supply(&m.mint);
    env.report_now(&m, supply).unwrap();
    assert_eq!(env.market(&m).risk, 0);
    env.buy(&alice, &m, 100 * USDC, 600_000).unwrap();
    // Only the owner sets a market's cap.
    let k = Env::as_kp(&env.keeper);
    expect_err(env.market_admin(&k, &m, px::instruction::SetMaxRisk { cap: 0 }), ExchangeError::OnlyOwner);
}

#[test]
fn total_risk_cap_holds_across_markets() {
    let (mut env, m) = setup();
    let m2 = env.create_market(pid(ID_B));
    let alice = env.user(10_000 * USDC);
    env.owner_ix(px::instruction::SetMaxTotalRisk { cap: 300 * USDC }).unwrap();
    env.buy(&alice, &m, 200 * USDC, 500_000).unwrap(); // ~399 shares, ~$199.5 at risk
    expect_err(env.buy(&alice, &m2, 250 * USDC, 500_000), ExchangeError::RiskCap); // ~$124.7 more
    env.buy(&alice, &m2, 100 * USDC, 500_000).unwrap(); // ~$99.75: $299.25 in all
    assert_eq!(env.config().total_risk, env.market(&m).risk + env.market(&m2).risk);
    // Hedging one frees room.
    let supply = env.supply(&m.mint);
    env.report_now(&m, supply).unwrap();
    assert_eq!(env.config().total_risk, env.market(&m2).risk);
    env.buy(&alice, &m2, 150 * USDC, 500_000).unwrap(); // ~$174.6 in all, refused before the hedge
}

#[test]
fn reports_ahead_of_burns_are_skipped_and_above_supply_clamped() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 100 * USDC, 600_000).unwrap();
    let supply = env.supply(&m.mint);
    env.report_now(&m, supply * 10).unwrap();
    assert_eq!(env.market(&m).backed, supply);
    let v = env.view_market(&m);
    env.report(&m, 1, v.burned_total + 5, v.minted_total).unwrap(); // seen > burned_total: skipped
    assert_eq!(env.market(&m).backed, supply);
    env.report(&m, 1, v.burned_total, v.minted_total + 1).unwrap(); // minted > minted_total: skipped
    assert_eq!(env.market(&m).backed, supply);
    // One `minted` per amount, as one `seen`.
    let k = Env::as_kp(&env.keeper);
    let accounts = px::accounts::KeeperBatch { keeper: k.pubkey(), config: env.config, event_authority: env.event_authority, program: px::ID };
    let mut metas = accounts.to_account_metas(None);
    metas.push(AccountMeta::new(m.market, false));
    metas.push(AccountMeta::new_readonly(m.mint, false));
    let ix = Instruction { program_id: px::ID, accounts: metas, data: px::instruction::ReportBacked { amounts: vec![1], seen: vec![0], minted: vec![] }.data() };
    expect_err(env.send(&[ix], &k, &[]), ExchangeError::BadParams);
    // Only the keeper reports.
    let o = Env::as_kp(&env.owner);
    let accounts = px::accounts::KeeperBatch { keeper: o.pubkey(), config: env.config, event_authority: env.event_authority, program: px::ID };
    let ix = Instruction {
        program_id: px::ID,
        accounts: accounts.to_account_metas(None),
        data: px::instruction::ReportBacked { amounts: vec![], seen: vec![], minted: vec![] }.data(),
    };
    expect_err(env.send(&[ix], &o, &[]), ExchangeError::OnlyKeeper);
}

/* ------------------------------------------------------------- outflow */

#[test]
fn sale_over_the_hourly_cap_is_delayed_then_released() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    env.flat_cap(100 * USDC);
    let mine = env.ptoken_account(&alice, &m);
    let shares = env.balance(&mine);
    let sell = 600 * USDC; // ~$299 of shares at 50c
    let before = env.balance(&alice.usdc);
    env.sell(&alice, &m, sell, 500_000).unwrap();
    let out = usdc_for(sell, 500_000, 25);
    // $100 now, the rest waits as ticket 0 with its pToken held.
    assert_eq!(env.balance(&alice.usdc) - before, 100 * USDC);
    let t = env.ticket_at(0).unwrap();
    let p_later = (sell as u128 * (out - 100 * USDC) as u128 / out as u128) as u64;
    assert_eq!((t.amount, t.p_amount, t.to, t.to_owner), (out - 100 * USDC, p_later, alice.usdc, alice.key()));
    assert_eq!(t.ready_at, env.now + DELAY);
    assert_eq!(env.balance(&m.held), p_later);
    assert_eq!(env.market(&m).delayed_shares, p_later);
    assert_eq!(env.balance(&mine), shares - sell);
    // Shares held for the delayed sale still count in supply.
    assert_eq!(env.supply(&m.mint), shares - sell + p_later);
    // Absorb leaves them alone.
    env.absorb(&m).unwrap();
    assert_eq!(env.balance(&m.held), p_later);

    expect_err(env.release(0), ExchangeError::NotYet);
    env.warp(DELAY);
    env.release(0).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before, out);
    assert!(env.ticket_at(0).is_none());
    assert_eq!(env.balance(&m.held), 0);
    assert_eq!(env.market(&m).delayed_shares, 0);
    assert_eq!(env.supply(&m.mint), shares - sell);
}

#[test]
fn quoted_sales_are_unmetered_when_meter_sales_is_off_but_payouts_stay_metered() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    env.flat_cap(100 * USDC);
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    let before = env.balance(&alice.usdc);
    env.sell(&alice, &m, 600 * USDC, 500_000).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before, usdc_for(600 * USDC, 500_000, 25));
    assert!(env.ticket_at(0).is_none());
    // A settled payout is metered whatever meter_sales says.
    let k = Env::as_kp(&env.keeper);
    env.settle(&k, &m, 1_000_000).unwrap();
    env.warp(SETTLE_DELAY);
    env.fund_vault(2_000 * USDC);
    let before = env.balance(&alice.usdc);
    env.redeem_at_payout(&alice, &m, 300 * USDC).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before, 100 * USDC);
    assert!(env.ticket_at(0).is_some());
    // Only the owner switches it.
    let ix = env.admin_ix(k.pubkey(), px::instruction::SetMeterSales { on: true });
    expect_err(env.send(&[ix], &k, &[]), ExchangeError::OnlyOwner);
}

#[test]
fn owner_cancels_a_delayed_sale_and_the_shares_go_back() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    env.flat_cap(100 * USDC);
    env.sell(&alice, &m, 600 * USDC, 500_000).unwrap();
    let t = env.ticket_at(0).unwrap();
    let mine = env.ptoken_account(&alice, &m);
    let had = env.balance(&mine);
    // Halted: release refused; the owner's cancel still works.
    let k = Env::as_kp(&env.keeper);
    env.halt_by(&k).unwrap();
    env.warp(DELAY);
    expect_err(env.release(0), ExchangeError::Halted);
    expect_err(env.cancel_by(&k, 0, mine), ExchangeError::OnlyOwner);
    // Only to the sale's payee.
    let bob = env.user(0);
    let bobs = env.ptoken_account(&bob, &m);
    let o = Env::as_kp(&env.owner);
    assert!(env.cancel_by(&o, 0, bobs).is_err());
    env.cancel_by(&o, 0, mine).unwrap();
    assert_eq!(env.balance(&mine), had + t.p_amount);
    assert!(env.ticket_at(0).is_none());
    assert_eq!(env.market(&m).delayed_shares, 0);
}

#[test]
fn dust_over_a_spent_cap_is_refused_and_under_a_dollar_is_paid() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    env.flat_cap(100 * USDC);
    // Just under $1 over the cap: paid in full now.
    let amount = 200 * USDC + 1_500_000;
    let before = env.balance(&alice.usdc);
    env.sell(&alice, &m, amount, 500_000).unwrap();
    assert!(env.ticket_at(0).is_none());
    // The meter counts all of it, the part over the cap included (Phase 5 review).
    let paid = env.balance(&alice.usdc) - before;
    assert!(paid > 100 * USDC);
    assert_eq!(env.config().outflow_used(env.now), paid);
    // The cap is spent: a small sale is refused, not ticketed.
    expect_err(env.sell(&alice, &m, 1_000_000, 500_000), ExchangeError::DustOverCap);
    // The sliding hour frees the cap as it passes.
    env.warp(2 * HOUR);
    env.sell(&alice, &m, 1_000_000, 500_000).unwrap();
}

/// Phase 5 review: the dust edge is exact. MIN_DELAYED - 1 over a cap not
/// yet spent is paid now; exactly MIN_DELAYED over is ticketed, for exactly
/// that, and the meter counts only what was paid now.
#[test]
fn exactly_min_delayed_over_the_cap_is_ticketed() {
    for (over, ticketed) in [(MIN_DELAYED - 1, false), (MIN_DELAYED, true)] {
        let (mut env, m) = setup();
        let alice = env.user(10_000 * USDC);
        env.uncapped();
        env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
        env.flat_cap(100 * USDC);
        // At 50c less 0.25%, this many shares pay exactly 100 USDC + `over`.
        let eff = 500_000u128 * 9_975 / 10_000;
        let target = 100 * USDC + over;
        let amount = ((target as u128 * ONE as u128).div_ceil(eff)) as u64;
        assert_eq!(amount as u128 * eff / ONE as u128, target as u128);
        let before = env.balance(&alice.usdc);
        env.sell(&alice, &m, amount, 500_000).unwrap();
        let paid = env.balance(&alice.usdc) - before;
        let t = env.ticket_at(0);
        assert_eq!(t.is_some(), ticketed, "{over} over the cap");
        if ticketed {
            assert_eq!((paid, t.unwrap().amount), (100 * USDC, MIN_DELAYED));
        } else {
            assert_eq!(paid, target);
        }
        assert_eq!(env.config().outflow_used(env.now), paid);
    }
}

/// FloatDrain F3: selling a cap just before an hour turns and another just
/// after cannot take two caps: the previous hour counts for what is left of it.
#[test]
fn hour_rollover_cannot_double_the_cap() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    env.flat_cap(100 * USDC);
    env.warp(HOUR - 1);
    env.sell(&alice, &m, 200 * USDC, 500_000).unwrap(); // ~$99.75, paid
    assert!(env.ticket_at(0).is_none());
    env.warp(2); // one second into the next hour
    let before = env.balance(&alice.usdc);
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap(); // ~$49.9
    // Only what the sliding hour has freed (about 3 cents) is paid now.
    assert!(env.balance(&alice.usdc) - before < USDC);
    assert!(env.ticket_at(0).is_some());
}

#[test]
fn cap_is_the_hours_opening_float() {
    let (mut env, m) = setup();
    let alice = env.user(100_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetOutflowCap { floor: 0, float_bps: 5_000 }).unwrap();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap(); // hour's snapshot: float 0 before this buy
    env.warp(HOUR);
    // New hour: base = min(now ~1,000, last snapshot 0) = 0 -> cap 0. Money
    // put in just before an hour turns cannot lift that hour's cap.
    env.buy(&alice, &m, USDC, 500_000).unwrap();
    expect_err(env.sell(&alice, &m, 1_000_000, 500_000), ExchangeError::DustOverCap);
    env.warp(HOUR);
    // Next hour: base = min(1,001, 1,000) -> $500.
    env.sell(&alice, &m, 1_000 * USDC, 500_000).unwrap(); // ~$498.75
    assert!(env.ticket_at(0).is_none());
    // Minting inside the hour does not raise it.
    env.buy(&alice, &m, 5_000 * USDC, 500_000).unwrap();
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    assert!(env.ticket_at(0).is_some());
}

/* --------------------------------------------------------------- queue */

#[test]
fn short_float_queues_claims_paid_in_order() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    env.buy(&alice, &m, 500 * USDC, 500_000).unwrap();
    env.buy(&bob, &m, 500 * USDC, 500_000).unwrap();
    // The keeper sends most of the float to the desk.
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, 900 * USDC).unwrap();
    // Alice sells for ~$399: $100 now, the rest queued.
    let a_out = usdc_for(800 * USDC, 500_000, 25);
    let before_a = env.balance(&alice.usdc);
    env.sell(&alice, &m, 800 * USDC, 500_000).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before_a, 100 * USDC);
    let c = env.config();
    assert_eq!((c.claim_head, c.claim_tail, c.queued), (0, 1, a_out - 100 * USDC));
    // The bridge can never touch what the queue is owed.
    expect_err(env.send_to_bridge_by(&k, bridge, 1), ExchangeError::InsufficientFloat);
    // Bob sells: all queued, behind alice.
    let b_out = usdc_for(100 * USDC, 500_000, 25);
    env.sell(&bob, &m, 100 * USDC, 500_000).unwrap();
    assert_eq!(env.config().claim_tail, 2);
    // Enough for bob but not alice: strict FIFO pays nobody.
    env.fund_vault(b_out);
    env.pay_queue(5).unwrap();
    assert_eq!(env.config().claim_head, 0);
    // Enough for both: paid oldest first, claims closed.
    env.fund_vault(a_out);
    env.pay_queue(5).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before_a, a_out);
    let c = env.config();
    assert_eq!((c.claim_head, c.queued), (2, 0));
    assert!(!env.exists(&pda(&[CLAIM_SEED, &0u64.to_le_bytes()])));
    // Every mint pays the queue first.
    let bob2 = env.user(1_000 * USDC);
    env.send_to_bridge_by(&k, bridge, env.config().free_float(env.balance(&env.vault.clone()))).unwrap();
    env.sell(&bob, &m, 10 * USDC, 500_000).unwrap();
    assert_eq!(env.config().claim_tail, 3);
    env.buy(&bob2, &m, 100 * USDC, 500_000).unwrap();
    assert_eq!(env.config().claim_head, 3);
}

#[test]
fn frozen_recipient_is_set_aside_not_stuck() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    env.buy(&alice, &m, 500 * USDC, 500_000).unwrap();
    env.buy(&bob, &m, 500 * USDC, 500_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, 1_000 * USDC).unwrap();
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    env.sell(&bob, &m, 100 * USDC, 500_000).unwrap();
    let owed = usdc_for(100 * USDC, 500_000, 25);
    env.freeze(&alice.usdc, true);
    env.fund_vault(2 * owed);
    env.pay_queue(5).unwrap();
    // Alice's claim moved aside; bob behind her was paid.
    let c0 = pda(&[CLAIM_SEED, &0u64.to_le_bytes()]);
    let cl = env.claim_at(&c0).unwrap();
    assert!(cl.set_aside);
    let c = env.config();
    assert_eq!((c.claim_head, c.unclaimed_total, c.queued), (2, owed, owed));
    assert_eq!(env.balance(&bob.usdc), 10_000 * USDC - 500 * USDC + owed);
    // Set-aside USDC is reserved: not free float.
    expect_err(env.send_to_bridge_by(&k, bridge, 1), ExchangeError::InsufficientFloat);
    // Paid only to its own account, and not while frozen.
    assert!(env.withdraw_unclaimed(c0).is_err());
    env.freeze(&alice.usdc, false);
    let before = env.balance(&alice.usdc);
    env.withdraw_unclaimed(c0).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before, owed);
    assert_eq!((env.config().queued, env.config().unclaimed_total), (0, 0));
    assert!(!env.exists(&c0));
}

#[test]
fn release_to_a_frozen_recipient_sets_the_payment_aside() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    env.flat_cap(100 * USDC);
    env.sell(&alice, &m, 600 * USDC, 500_000).unwrap();
    let t = env.ticket_at(0).unwrap();
    env.warp(DELAY);
    env.freeze(&alice.usdc, true);
    let aside = env.next_aside();
    env.release(0).unwrap();
    let cl = env.claim_at(&aside).unwrap();
    assert!(cl.set_aside && cl.amount == t.amount && cl.to == alice.usdc);
    assert_eq!(env.config().unclaimed_total, t.amount);
    env.freeze(&alice.usdc, false);
    env.withdraw_unclaimed(aside).unwrap();
}

#[test]
fn a_prefunded_claim_address_cannot_block_the_queue() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    env.buy(&alice, &m, 500 * USDC, 500_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, 500 * USDC).unwrap();
    let next = env.next_claim();
    // Someone sends lamports (under the claim's rent) to the next claim's address.
    env.svm.airdrop(&next, 1_000_000).unwrap();
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    assert_eq!(env.claim_at(&next).unwrap().amount, usdc_for(100 * USDC, 500_000, 25));
}

/* -------------------------------------------------------------- bridge */

#[test]
fn send_to_bridge_bounds() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 100 * USDC, 600_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let o = Env::as_kp(&env.owner);
    let bridge = env.bridge;
    expect_err(env.send_to_bridge_by(&k, bridge, 100 * USDC + 1), ExchangeError::InsufficientFloat);
    expect_err(env.send_to_bridge_by(&o, bridge, 1), ExchangeError::OnlyKeeper);
    // Only to the configured deposit account.
    let elsewhere = Pubkey::new_unique();
    let (mint, kk) = (env.usdc_mint, k.pubkey());
    env.set_token(elsewhere, mint, kk, 0);
    expect_err(env.send_to_bridge_by(&k, elsewhere, 1), ExchangeError::WrongAccount);
    env.send_to_bridge_by(&k, bridge, 100 * USDC).unwrap();
    assert_eq!(env.balance(&bridge), 100 * USDC);
    // Cleared: no bridge at all.
    env.owner_ix(px::instruction::SetRoles { keeper: kk, factory: Pubkey::default(), bridge_deposit: Pubkey::default() }).unwrap();
    expect_err(env.send_to_bridge_by(&k, bridge, 0), ExchangeError::NoBridge);
}

#[test]
fn a_new_bridge_deposit_waits_two_days() {
    let (mut env, _) = setup();
    let k = env.keeper.pubkey();
    let next = Pubkey::new_unique();
    expect_err(
        env.owner_ix(px::instruction::SetRoles { keeper: k, factory: Pubkey::default(), bridge_deposit: next }),
        ExchangeError::BridgeChangeDelayed,
    );
    // Clearing then setting cannot skip the delay either.
    env.owner_ix(px::instruction::SetRoles { keeper: k, factory: Pubkey::default(), bridge_deposit: Pubkey::default() }).unwrap();
    expect_err(
        env.owner_ix(px::instruction::SetRoles { keeper: k, factory: Pubkey::default(), bridge_deposit: next }),
        ExchangeError::BridgeChangeDelayed,
    );
    expect_err(env.owner_ix(px::instruction::AcceptBridgeDeposit {}), ExchangeError::NothingPending);
    env.owner_ix(px::instruction::ProposeBridgeDeposit { bridge_deposit: next }).unwrap();
    env.warp(BRIDGE_DELAY - 1);
    expect_err(env.owner_ix(px::instruction::AcceptBridgeDeposit {}), ExchangeError::NotYet);
    env.warp(1);
    env.owner_ix(px::instruction::AcceptBridgeDeposit {}).unwrap();
    assert_eq!(env.config().bridge_deposit, next);
}

/* --------------------------------------------------------------- roles */

#[test]
fn ownership_moves_in_two_steps_and_cannot_be_renounced() {
    let (mut env, _) = setup();
    let new_owner = Keypair::new();
    env.svm.airdrop(&new_owner.pubkey(), 1_000_000_000).unwrap();
    let stranger = Keypair::new();
    env.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    env.owner_ix(px::instruction::TransferOwnership { new_owner: new_owner.pubkey() }).unwrap();
    // Still the old owner until accepted.
    assert_eq!(env.config().owner, env.owner.pubkey());
    let accept = |env: &Env, who: &Keypair| {
        let accounts = px::accounts::AcceptOwnership {
            pending_owner: who.pubkey(),
            config: env.config,
            event_authority: env.event_authority,
            program: px::ID,
        };
        Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::AcceptOwnership {}.data() }
    };
    let ix = accept(&env, &stranger);
    expect_err(env.send(&[ix], &stranger, &[]), ExchangeError::NotPendingOwner);
    let ix = accept(&env, &new_owner);
    env.send(&[ix], &new_owner, &[]).unwrap();
    assert_eq!(env.config().owner, new_owner.pubkey());
    expect_err(env.owner_ix(px::instruction::SetMeterSales { on: false }), ExchangeError::OnlyOwner);
    // Handing it to nobody leaves it pending with a key no one holds.
    let ix = env.admin_ix(new_owner.pubkey(), px::instruction::TransferOwnership { new_owner: Pubkey::default() });
    env.send(&[ix], &new_owner, &[]).unwrap();
    assert_eq!(env.config().owner, new_owner.pubkey());
}

#[test]
fn a_new_signer_waits_twelve_hours_and_revoking_is_immediate() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let next = SigningKey::from_bytes(&[9; 32]);
    let next_pk = Pubkey::new_from_array(next.verifying_key().to_bytes());
    env.owner_ix(px::instruction::SetSigner { signer: next_pk }).unwrap();
    // The new key's quotes do not verify yet; the old key's still do.
    let q = env.quote(&m, BUY, 600_000, 10 * USDC);
    let to = env.ptoken_account(&alice, &m);
    let ix = env.mint_ix(&alice.key(), &alice.usdc, &to, &m, 10 * USDC, 0, &q);
    let by_next = Env::ed25519_ix_by(&next, &q.message(&px::ID));
    let kp = Env::as_kp(&alice.kp);
    expect_err(env.send(&[by_next, ix.clone()], &kp, &[]), ExchangeError::BadSignature);
    env.buy(&alice, &m, 10 * USDC, 600_000).unwrap();
    env.warp(ROLE_DELAY - 1);
    expect_err(env.owner_ix(px::instruction::AcceptSigner {}), ExchangeError::NotYet);
    env.warp(1);
    env.owner_ix(px::instruction::AcceptSigner {}).unwrap();
    assert_eq!(env.config().signer, next_pk);
    let q = env.quote(&m, BUY, 600_000, 10 * USDC);
    let ix = env.mint_ix(&alice.key(), &alice.usdc, &to, &m, 10 * USDC, 0, &q);
    expect_err(env.send(&[env.ed25519_ix(&q), ix.clone()], &kp, &[]), ExchangeError::BadSignature);
    env.send(&[Env::ed25519_ix_by(&next, &q.message(&px::ID)), ix], &kp, &[]).unwrap();
    // A proposal not accepted within ACCEPT_WINDOW lapses.
    let old = env.signer_key();
    env.owner_ix(px::instruction::SetSigner { signer: old }).unwrap();
    env.warp(ROLE_DELAY + ACCEPT_WINDOW + 1);
    expect_err(env.owner_ix(px::instruction::AcceptSigner {}), ExchangeError::NothingPending);
    // Revoking is immediate: no quote verifies.
    env.owner_ix(px::instruction::SetSigner { signer: Pubkey::default() }).unwrap();
    let q = env.quote(&m, BUY, 600_000, 10 * USDC);
    let ix = env.mint_ix(&alice.key(), &alice.usdc, &to, &m, 10 * USDC, 0, &q);
    expect_err(env.send(&[Env::ed25519_ix_by(&next, &q.message(&px::ID)), ix], &kp, &[]), ExchangeError::NoSigner);
}

#[test]
fn setters_keep_the_evm_bounds() {
    let (mut env, _) = setup();
    let p = |b, s, lo, hi| px::instruction::SetParams { buy_spread_bps: b, sell_spread_bps: s, min_price: lo, max_price: hi, default_max_risk: 1 };
    expect_err(env.owner_ix(p(501, 25, 1_000, 999_000)), ExchangeError::BadParams);
    expect_err(env.owner_ix(p(25, 501, 1_000, 999_000)), ExchangeError::BadParams);
    expect_err(env.owner_ix(p(25, 25, 999, 999_000)), ExchangeError::BadParams);
    expect_err(env.owner_ix(p(25, 25, 500_000, 500_000)), ExchangeError::BadParams);
    expect_err(env.owner_ix(p(25, 25, 1_000, 1_000_000)), ExchangeError::BadParams);
    env.owner_ix(p(500, 500, 1_000, 999_999)).unwrap();
    expect_err(env.owner_ix(px::instruction::SetSettleFee { settle_fee_bps: 201 }), ExchangeError::BadParams);
    env.owner_ix(px::instruction::SetSettleFee { settle_fee_bps: 200 }).unwrap();
    expect_err(env.owner_ix(px::instruction::SetOutflowCap { floor: 0, float_bps: 10_001 }), ExchangeError::BadParams);
    env.owner_ix(px::instruction::SetOutflowCap { floor: 0, float_bps: 10_000 }).unwrap();
    let c = env.config();
    assert_eq!((c.buy_spread_bps, c.settle_fee_bps, c.outflow_float_bps), (500, 200, 10_000));
}

#[test]
fn defaults_match_the_evm_constructor() {
    let env = Env::new();
    let c = env.config();
    assert_eq!((c.buy_spread_bps, c.sell_spread_bps, c.settle_fee_bps), (25, 25, 50));
    assert_eq!((c.min_price, c.max_price), (1_000, 999_000));
    assert!(c.meter_sales && !c.halted);
    assert_eq!((c.default_max_risk, c.max_total_risk), (5_000 * USDC, 25_000 * USDC));
    assert_eq!((c.outflow_floor, c.outflow_float_bps), (5_000 * USDC, 5_000));
    assert_eq!(c.usdc_mint, env.usdc_mint);
    assert!(c.bridge_ever_set);
}

#[test]
fn only_the_upgrade_authority_initializes_and_only_once() {
    let mut env = Env::bare();
    let squatter = Keypair::new();
    env.svm.airdrop(&squatter.pubkey(), 10_000_000_000).unwrap();
    expect_err(env.initialize(&squatter), ExchangeError::OnlyOwner);
    let o = Env::as_kp(&env.owner);
    env.initialize(&o).unwrap();
    assert!(env.initialize(&o).is_err());
}

#[test]
fn markets_are_created_by_the_factory_or_owner_once() {
    let mut env = Env::new();
    let stranger = Keypair::new();
    env.svm.airdrop(&stranger.pubkey(), 10_000_000_000).unwrap();
    assert!(env.create_market_by(&stranger, pid(ID_A)).is_err());
    let factory = Keypair::new();
    env.svm.airdrop(&factory.pubkey(), 10_000_000_000).unwrap();
    let (k, b) = (env.keeper.pubkey(), env.bridge);
    env.owner_ix(px::instruction::SetRoles { keeper: k, factory: factory.pubkey(), bridge_deposit: b }).unwrap();
    let m = env.create_market_by(&factory, pid(ID_A)).unwrap();
    // A second call is a no-op, not an error (the launch program calls it per coin).
    env.create_market_by(&factory, pid(ID_A)).unwrap();
    let mk = env.market(&m);
    assert_eq!((mk.mint, mk.held, mk.position_id), (m.mint, m.held, pid(ID_A)));
    // Classic SPL Token, 6 decimals, minted by the exchange PDA, no freeze authority.
    let mint = spl_token::state::Mint::unpack_from_slice(&env.svm.get_account(&m.mint).unwrap().data).unwrap();
    assert_eq!(env.svm.get_account(&m.mint).unwrap().owner, spl_token::ID);
    assert_eq!(mint.decimals, 6);
    assert_eq!(mint.mint_authority, anchor_lang::solana_program::program_option::COption::Some(env.config));
    assert!(mint.freeze_authority.is_none());
    // Position id 0 never trades.
    assert!(env.create_market_by(&factory, [0; 32]).is_err());
}

use anchor_lang::solana_program::program_pack::Pack;

#[test]
fn rescue_never_takes_usdc_or_a_ptoken() {
    let (mut env, m) = setup();
    let o = Env::as_kp(&env.owner);
    let rescue = |env: &Env, source: Pubkey, mint: Pubkey, dest: Pubkey| {
        let accounts = px::accounts::Rescue {
            owner: o.pubkey(),
            config: env.config,
            source,
            mint,
            dest,
            token_program: spl_token::ID,
            event_authority: env.event_authority,
            program: px::ID,
        };
        Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::Rescue { amount: 1 }.data() }
    };
    let mine = Pubkey::new_unique();
    let (usdc, oo, cfg) = (env.usdc_mint, o.pubkey(), env.config);
    env.set_token(mine, usdc, oo, 0);
    let ix = rescue(&env, env.vault, usdc, mine);
    expect_err(env.send(&[ix], &o, &[]), ExchangeError::NotRescuable);
    // A stray USDC account the exchange owns is not rescuable either.
    let stray = Pubkey::new_unique();
    env.set_token(stray, usdc, cfg, 5);
    let ix = rescue(&env, stray, usdc, mine);
    expect_err(env.send(&[ix], &o, &[]), ExchangeError::NotRescuable);
    let my_p = Pubkey::new_unique();
    env.set_token(my_p, m.mint, oo, 0);
    let ix = rescue(&env, m.held, m.mint, my_p);
    expect_err(env.send(&[ix], &o, &[]), ExchangeError::NotRescuable);
    // Anything else comes back.
    let other_mint = Pubkey::new_unique();
    env.set_mint(other_mint, 9, Some(oo), None);
    let src = Pubkey::new_unique();
    let dst = Pubkey::new_unique();
    env.set_token(dst, other_mint, oo, 0);
    let ix = spl_token::instruction::initialize_account3(&spl_token::ID, &src, &other_mint, &cfg).unwrap();
    let _ = ix;
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account {
        mint: other_mint,
        owner: cfg,
        amount: 7,
        delegate: anchor_lang::solana_program::program_option::COption::None,
        state: spl_token::state::AccountState::Initialized,
        is_native: anchor_lang::solana_program::program_option::COption::None,
        delegated_amount: 0,
        close_authority: anchor_lang::solana_program::program_option::COption::None,
    }
    .pack_into_slice(&mut data);
    let lamports = env.svm.minimum_balance_for_rent_exemption(data.len());
    env.svm.set_account(src, solana_account::Account { lamports, data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
    let ix = rescue(&env, src, other_mint, dst);
    env.send(&[ix], &o, &[]).unwrap();
    assert_eq!(env.balance(&dst), 1);
}

#[test]
fn absorb_burns_shares_sent_to_the_exchange() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let mine = env.ptoken_account(&alice, &m);
    let ix = spl_token::instruction::transfer(&spl_token::ID, &mine, &m.held, &alice.key(), &[], 1_000_000).unwrap();
    env.send(&[ix], &Env::as_kp(&alice.kp), &[]).unwrap();
    let supply = env.supply(&m.mint);
    env.absorb(&m).unwrap();
    assert_eq!(env.supply(&m.mint), supply - 1_000_000);
    assert_eq!(env.market(&m).burned_total, 1_000_000);
    assert_eq!(env.balance(&m.held), 0);
}

#[test]
fn fills_close_only_after_expiry_and_refund_the_payer() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    let q = env.quote(&m, BUY, 600_000, 100 * USDC);
    env.buy_with(&alice, &m, 10 * USDC, &q).unwrap();
    let fill = pda(&[FILL_SEED, &q.fill_seed()]);
    let close = |_: &Env| {
        let accounts = px::accounts::CloseFill { fill, payer: alice.key() };
        Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::CloseFill {}.data() }
    };
    let anyone = Env::as_kp(&env.usdc_auth);
    env.warp(15 + px::quote::MAX_VALIDITY);
    expect_err(env.send(&[close(&env)], &anyone, &[]), ExchangeError::FillLive);
    let before = env.svm.get_balance(&alice.key()).unwrap();
    env.warp(1);
    env.send(&[close(&env)], &anyone, &[]).unwrap();
    assert!(!env.exists(&fill));
    assert!(env.svm.get_balance(&alice.key()).unwrap() > before);
    // The quote is long expired: it cannot be replayed into a fresh fill.
    expect_err(env.buy_with(&alice, &m, 10 * USDC, &q), ExchangeError::QuoteExpired);
}

/* --------------------------------------------- direct burns (Solana only) */

/// The books after any exchange instruction on `m`: every burn counted,
/// backing within the supply.
#[track_caller]
fn books_hold(env: &Env, m: &Mkt) {
    let mk = env.market(m);
    let supply = env.supply(&m.mint);
    assert_eq!(mk.burned_total + supply, mk.minted_total, "burned_total + supply != minted_total");
    assert!(mk.backed <= supply, "backed {} > supply {}", mk.backed, supply);
}

/// Fully hedged, as the keeper reports it, `seen` read from the view.
fn hedge_all(env: &mut Env, m: &Mkt) {
    let v = env.view_market(m);
    env.report(m, v.supply, v.burned_total, v.minted_total).unwrap();
    assert_eq!(env.market(m).backed, env.supply(&m.mint));
}

/// Phase 1 review (high): on SPL Token any holder can burn their own pTokens,
/// which on the EVM only the exchange could do. A dust burn right after a
/// full hedge used to make every later redeem, release and absorb on the
/// market fail with MathOverflow until the next report.
#[test]
fn a_direct_burn_cannot_freeze_redeem_release_or_absorb() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    env.uncapped();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    env.buy(&bob, &m, 100 * USDC, 500_000).unwrap();

    // Redeem.
    hedge_all(&mut env, &m);
    env.burn_direct(&bob, &m, 1).unwrap();
    assert!(env.market(&m).backed > env.supply(&m.mint));
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    books_hold(&env, &m);

    // Release.
    env.flat_cap(100 * USDC);
    env.sell(&alice, &m, 600 * USDC, 500_000).unwrap();
    assert!(env.ticket_at(0).is_some());
    hedge_all(&mut env, &m);
    env.burn_direct(&bob, &m, 1).unwrap();
    env.warp(DELAY);
    env.release(0).unwrap();
    books_hold(&env, &m);

    // Absorb.
    hedge_all(&mut env, &m);
    env.burn_direct(&bob, &m, 1).unwrap();
    let held = m.held;
    env.transfer_ptoken(&alice, &m, held, USDC).unwrap();
    env.absorb(&m).unwrap();
    books_hold(&env, &m);

    // Redeem at a payout.
    let k = Env::as_kp(&env.keeper);
    env.settle(&k, &m, 1_000_000).unwrap();
    env.warp(SETTLE_DELAY);
    env.flat_cap(1_000_000 * USDC);
    env.fund_vault(1_000 * USDC);
    hedge_all(&mut env, &m);
    env.burn_direct(&bob, &m, 1).unwrap();
    env.redeem_at_payout(&alice, &m, 10 * USDC).unwrap();
    books_hold(&env, &m);
    // Burning everything one holds is fine too.
    let bobs = env.ptoken_account(&bob, &m);
    let all = env.balance(&bobs);
    env.burn_direct(&bob, &m, all).unwrap();
    env.redeem_at_payout(&alice, &m, USDC).unwrap();
    books_hold(&env, &m);
}

#[test]
fn direct_burns_between_mint_report_and_refresh() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    hedge_all(&mut env, &m);
    env.burn_direct(&alice, &m, 1).unwrap();

    // Mint: the market counts as fully backed, so the risk starts afresh.
    env.buy(&bob, &m, 100 * USDC, 500_000).unwrap();
    books_hold(&env, &m);
    let out = shares_for(100 * USDC, 500_000);
    let mk = env.market(&m);
    assert_eq!((mk.risk, mk.minted_since_report), (out as u128 * 500_000, out));
    assert_eq!(env.config().total_risk, mk.risk);

    // A direct burn changes nothing until an instruction books it...
    let risk = env.market(&m).risk;
    env.burn_direct(&bob, &m, out / 2).unwrap();
    assert_eq!(env.market(&m).risk, risk);
    // ...then it counts against minted_since_report first and takes their
    // risk off, as an exchange redemption of the same shares would, and no
    // more (here at a refresh).
    env.refresh_risk(&[&m]).unwrap();
    books_hold(&env, &m);
    let mk = env.market(&m);
    assert_eq!(mk.minted_since_report, out - out / 2);
    assert_eq!(mk.risk, (out - out / 2) as u128 * 500_000);
    assert_eq!(env.config().total_risk, mk.risk);

    // Report: a keeper that read its holdings before a direct burn (counts
    // from before it) is held to the supply the burn left, like any burn:
    // the desk may have sold down to it.
    let v = env.view_market(&m);
    env.burn_direct(&alice, &m, 5 * USDC).unwrap();
    env.report(&m, v.supply, v.burned_total, v.minted_total).unwrap();
    assert_eq!(env.market(&m).backed, v.supply - 5 * USDC);
    books_hold(&env, &m);
    // A keeper that reads seen from the view after the burn is not skipped.
    let v = env.view_market(&m);
    env.report(&m, v.supply, v.burned_total, v.minted_total).unwrap();
    assert_eq!(env.market(&m).backed, v.supply);
    // seen ahead of every burn, direct ones included, is still skipped.
    env.report(&m, 0, v.burned_total + 1, v.minted_total).unwrap();
    assert_eq!(env.market(&m).backed, v.supply);
}

/// Phase 1 review (medium): pToken sent to the exchange PDA's associated
/// token account (the natural Solana "send to the exchange") is absorbed,
/// as the EVM absorbs any pToken the exchange holds.
#[test]
fn absorb_burns_shares_sent_to_the_exchanges_associated_account() {
    let (mut env, m) = setup();
    let alice = env.user(1_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let ata = env.exchange_ata(&m);
    let held = m.held;
    env.transfer_ptoken(&alice, &m, ata, 3 * USDC).unwrap();
    env.transfer_ptoken(&alice, &m, held, USDC).unwrap();
    let supply = env.supply(&m.mint);
    env.absorb_with(&m, Some(ata)).unwrap();
    assert_eq!(env.supply(&m.mint), supply - 4 * USDC);
    assert_eq!((env.balance(&ata), env.balance(&m.held)), (0, 0));
    assert_eq!(env.market(&m).burned_total, 4 * USDC);
    books_hold(&env, &m);
    // Only an account the exchange PDA owns, of this market's pToken, and not `held` twice.
    let mine = env.ptoken_account(&alice, &m);
    // A holder's own account: refused by `token::authority = config`.
    expect_custom(env.absorb_with(&m, Some(mine)), anchor_code(AnchorError::ConstraintTokenOwner));
    // `held` again: Anchor refuses the same writable account twice before
    // the program's own `other != held` check would run.
    expect_custom(env.absorb_with(&m, Some(held)), anchor_code(AnchorError::ConstraintDuplicateMutableAccount));
    // The exchange's account for another market: refused by `token::mint`.
    let m2 = env.create_market(pid(ID_B));
    let other_ata = env.exchange_ata(&m2);
    expect_custom(env.absorb_with(&m, Some(other_ata)), anchor_code(AnchorError::ConstraintTokenMint));
    // Nothing there: a no-op.
    env.absorb_with(&m, Some(ata)).unwrap();
}

/* --------------------------------------------------- queue (Solana only) */

/// Phase 1 review (low): every EVM mint and payment runs `_payQueue(3)`. Here
/// the client passes the claims, so a live queue requires at least its head.
#[test]
fn mints_and_sales_must_offer_the_queue_head() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, 1_000 * USDC).unwrap();
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    assert_eq!(env.config().claim_tail, 2);

    // A mint with no queue accounts is refused.
    let q = env.quote(&m, BUY, 500_000, 100 * USDC);
    let to = env.ptoken_account(&bob, &m);
    let mut ix = env.mint_ix(&bob.key(), &bob.usdc, &to, &m, 100 * USDC, 0, &q);
    let base = ix.accounts.len() - 6;
    let queue = ix.accounts.split_off(base);
    let kp = Env::as_kp(&bob.kp);
    expect_err(env.send(&[env.ed25519_ix(&q), ix.clone()], &kp, &[]), ExchangeError::MissingAccount);
    // So is a sale.
    let sq = env.quote(&m, SELL, 500_000, USDC);
    let mut rix = env.redeem_ix(&alice, &m, USDC, 0, &sq, &alice.usdc);
    rix.accounts.truncate(rix.accounts.len() - 6);
    let akp = Env::as_kp(&alice.kp);
    expect_err(env.send(&[env.ed25519_ix(&sq), rix], &akp, &[]), ExchangeError::MissingAccount);

    // With the head: fine, and the mint's $100 pays it (~$49.9).
    let mut with_head = ix.clone();
    with_head.accounts.extend(queue[..3].iter().cloned());
    env.send(&[env.ed25519_ix(&q), with_head], &kp, &[]).unwrap();
    assert_eq!(env.config().claim_head, 1);

    // A stale list (claim 0, paid and closed since, then claim 1) still works.
    let q = env.quote(&m, BUY, 500_000, 100 * USDC);
    let mut stale = env.mint_ix(&bob.key(), &bob.usdc, &to, &m, 100 * USDC, 0, &q);
    stale.accounts.truncate(base);
    stale.accounts.extend(queue.iter().cloned());
    env.send(&[env.ed25519_ix(&q), stale], &kp, &[]).unwrap();
    assert_eq!(env.config().claim_head, 2);
    // An empty queue needs nothing.
    let q = env.quote(&m, BUY, 500_000, 10 * USDC);
    let mut none = env.mint_ix(&bob.key(), &bob.usdc, &to, &m, 10 * USDC, 0, &q);
    none.accounts.truncate(base);
    env.send(&[env.ed25519_ix(&q), none], &kp, &[]).unwrap();
}

/// Phase 2 review: two claims in line, claim 0's recipient frozen. Another
/// transaction sets claim 0 aside (head = 1) after a client read the list
/// [claim 0, claim 1]; that list still pays claim 1.
#[test]
fn a_stale_list_skips_a_claim_set_aside_since() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    let carol = env.user(10_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    env.buy(&alice, &m, 500 * USDC, 500_000).unwrap();
    env.buy(&bob, &m, 500 * USDC, 500_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, 1_000 * USDC).unwrap();
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    env.sell(&bob, &m, 100 * USDC, 500_000).unwrap();
    let owed = usdc_for(100 * USDC, 500_000, 25);
    assert_eq!(env.config().claim_tail, 2);
    let read = env.queue_metas(QUEUE_BATCH);

    // Only claim 0's USDC: it is set aside (alice frozen), claim 1 waits.
    env.freeze(&alice.usdc, true);
    env.fund_vault(owed);
    env.pay_queue(1).unwrap();
    let c = env.config();
    assert_eq!((c.claim_head, c.unclaimed_total), (1, owed));

    // Carol's mint brings the USDC and passes the list as read: claim 0 is
    // skipped and claim 1 paid.
    let before = env.balance(&bob.usdc);
    *env.queue_list.borrow_mut() = Some(read);
    env.buy(&carol, &m, 100 * USDC, 500_000).unwrap();
    assert_eq!(env.config().claim_head, 2);
    assert_eq!(env.balance(&bob.usdc) - before, owed);
    assert!(env.claim_at(&pda(&[CLAIM_SEED, &0u64.to_le_bytes()])).unwrap().set_aside);
}

/// Phase 2 review: another transaction pays the head between a client's read
/// and its send. A client that passed only the head triple is refused (the
/// program cannot tell a claim it paid from any other closed account, and
/// counting one as the head would let anyone skip the queue); one that
/// passed the next three still goes through. The SDK therefore attaches the
/// next three, and re-reads and resends on `MissingAccount`.
#[test]
fn a_list_of_only_the_head_fails_once_another_pays_it_first() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, 1_000 * USDC).unwrap();
    for _ in 0..3 {
        env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    }
    let owed = usdc_for(100 * USDC, 500_000, 25);
    let read = env.queue_metas(QUEUE_BATCH);
    assert_eq!(read.len(), 9);

    // Someone pays claim 0 first.
    env.fund_vault(owed);
    env.pay_queue(1).unwrap();
    assert_eq!(env.config().claim_head, 1);

    // Only the head as read: refused.
    *env.queue_list.borrow_mut() = Some(read[..3].to_vec());
    expect_err(env.buy(&bob, &m, 100 * USDC, 500_000), ExchangeError::MissingAccount);
    *env.queue_list.borrow_mut() = Some(read[..3].to_vec());
    expect_err(env.sell(&alice, &m, USDC, 500_000), ExchangeError::MissingAccount);
    // The next three as read: claim 0 skipped, claim 1 (now the head) paid
    // from the $60 this mint brings.
    *env.queue_list.borrow_mut() = Some(read.clone());
    env.buy(&bob, &m, 60 * USDC, 500_000).unwrap();
    assert_eq!(env.config().claim_head, 2);
    // Two of the three paid since the read: the last is the head, still fine.
    *env.queue_list.borrow_mut() = Some(read);
    env.buy(&bob, &m, 60 * USDC, 500_000).unwrap();
    assert_eq!(env.config().claim_head, 3);
}

/// Phase 4 review: a release pays like a sale, so it needs the head too, and
/// a claim set aside behind the head, like one paid and closed, never stands
/// in for it (for a release, a mint or a sale).
#[test]
fn a_release_or_a_list_of_only_a_set_aside_claim_needs_the_head() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    let carol = env.user(10_000 * USDC);
    env.uncapped();
    for u in [&alice, &bob, &carol] {
        env.buy(u, &m, 1_000 * USDC, 500_000).unwrap();
    }
    // Alice's sale over the cap leaves ticket 0.
    env.flat_cap(100 * USDC);
    env.sell(&alice, &m, 600 * USDC, 500_000).unwrap();
    assert!(env.ticket_at(0).is_some());
    // With the float at the desk, three unmetered sales queue claims 0 (bob), 1 (carol), 2 (bob).
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    let free = env.config().free_float(env.balance(&env.vault.clone()));
    env.send_to_bridge_by(&k, bridge, free).unwrap();
    for u in [&bob, &carol, &bob] {
        env.sell(u, &m, 100 * USDC, 500_000).unwrap();
    }
    assert_eq!(env.config().claim_tail, 3);
    let owed = usdc_for(100 * USDC, 500_000, 25);
    let read = env.queue_metas(QUEUE_BATCH);
    env.warp(DELAY);

    // Claim 0 paid since the read: a release with no list, or with only that
    // closed claim, is refused.
    env.fund_vault(owed);
    env.pay_queue(1).unwrap();
    assert_eq!(env.config().claim_head, 1);
    *env.queue_list.borrow_mut() = Some(vec![]);
    expect_err(env.release(0), ExchangeError::MissingAccount);
    *env.queue_list.borrow_mut() = Some(read[..3].to_vec());
    expect_err(env.release(0), ExchangeError::MissingAccount);

    // Claim 1 set aside since (carol frozen): a list of only it is refused
    // for a release, a mint and a sale alike.
    env.freeze(&carol.usdc, true);
    env.fund_vault(owed);
    env.pay_queue(1).unwrap();
    assert!(env.claim_at(&pda(&[CLAIM_SEED, &1u64.to_le_bytes()])).unwrap().set_aside);
    assert_eq!(env.config().claim_head, 2);
    *env.queue_list.borrow_mut() = Some(read[3..6].to_vec());
    expect_err(env.release(0), ExchangeError::MissingAccount);
    *env.queue_list.borrow_mut() = Some(read[3..6].to_vec());
    expect_err(env.buy(&alice, &m, 10 * USDC, 500_000), ExchangeError::MissingAccount);
    *env.queue_list.borrow_mut() = Some(read[3..6].to_vec());
    expect_err(env.sell(&alice, &m, USDC, 500_000), ExchangeError::MissingAccount);

    // The list as read reaches claim 2, the head: the release goes through.
    *env.queue_list.borrow_mut() = Some(read);
    env.release(0).unwrap();
    assert!(env.ticket_at(0).is_none());
}

/// Found by tests/invariants.rs: a sale that pays the seller's own older
/// claim (its rent back to the seller) and then queues the rest (the seller
/// paying the new claim's rent) used to fail as an unbalanced instruction,
/// because the claim was closed before the CPI that creates the new one.
#[test]
fn a_sale_can_pay_the_sellers_own_claim_and_queue_the_rest() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.uncapped();
    env.owner_ix(px::instruction::SetMeterSales { on: false }).unwrap();
    env.buy(&alice, &m, 1_000 * USDC, 500_000).unwrap();
    let k = Env::as_kp(&env.keeper);
    let bridge = env.bridge;
    env.send_to_bridge_by(&k, bridge, 1_000 * USDC).unwrap();
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    let owed = env.config().queued;
    // Just enough for the old claim, nothing for the new sale.
    env.fund_vault(owed);
    let before = env.balance(&alice.usdc);
    env.sell(&alice, &m, 100 * USDC, 500_000).unwrap();
    assert_eq!(env.balance(&alice.usdc) - before, owed);
    let c = env.config();
    assert_eq!((c.claim_head, c.claim_tail, c.queued), (1, 2, usdc_for(100 * USDC, 500_000, 25)));
    assert!(!env.exists(&pda(&[CLAIM_SEED, &0u64.to_le_bytes()])));
}

/* --------------------------------------------------------------- views */

#[test]
fn views_return_what_the_program_computes() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let f = env.view_float();
    let c = env.config();
    assert_eq!((f.vault, f.free_float, f.queued), (600 * USDC, 600 * USDC, 0));
    assert_eq!((f.claim_head, f.claim_tail, f.delayed_next), (0, 0, 0));
    assert_eq!(f.outflow_remaining, c.outflow_remaining(env.now, 600 * USDC));
    assert_eq!((f.total_risk, f.max_total_risk, f.halted), (c.total_risk, 25_000 * USDC, false));

    let v = env.view_market(&m);
    let mk = env.market(&m);
    let out = shares_for(600 * USDC, 600_000);
    assert_eq!((v.supply, v.backed, v.minted_total, v.burned_total), (out, 0, out, 0));
    assert_eq!((v.risk, v.max_risk), (mk.risk, 5_000 * USDC));
    assert!(!v.paused && !v.settled && !v.mints_at_payout);
    // A direct burn shows at once in the view, before any instruction records it.
    env.burn_direct(&alice, &m, USDC).unwrap();
    let v = env.view_market(&m);
    assert_eq!((v.supply, v.burned_total), (out - USDC, USDC));
    assert_eq!(env.market(&m).burned_total, 0);
    // A pending $1 payout: mints at the payout, not yet settled.
    let k = Env::as_kp(&env.keeper);
    env.settle(&k, &m, 1_000_000).unwrap();
    let v = env.view_market(&m);
    assert!(v.paused && !v.settled && v.mints_at_payout);
    assert_eq!((v.payout, v.settle_at), (1_000_000, env.now + SETTLE_DELAY));

    assert_eq!(env.preview(px::instruction::PreviewMint { price: 600_000, usdc_in: 600 * USDC }), out);
    assert_eq!(
        env.preview(px::instruction::PreviewRedeem { price: 590_000, amount_in: out, settled: false }),
        usdc_for(out, 590_000, 25)
    );
    assert_eq!(
        env.preview(px::instruction::PreviewRedeem { price: 1_000_000, amount_in: out, settled: true }),
        usdc_for(out, 1_000_000, 50)
    );
}

#[test]
fn initialize_refuses_a_usdc_mint_without_six_decimals() {
    let mut env = Env::bare();
    let (mint, auth) = (env.usdc_mint, env.usdc_auth.pubkey());
    env.set_mint(mint, 9, Some(auth), None);
    let o = Env::as_kp(&env.owner);
    expect_err(env.initialize(&o), ExchangeError::BadParams);
}

/// Phase 7 review (high, the reviewer's probe): a skipped report (a read
/// ahead of the market) with a direct burn of shares minted since the last
/// report pending. The visit books the burn into the market and the total
/// alike, and stores both: three skipped reports take its risk off once.
#[test]
fn a_skipped_report_keeps_total_risk_the_sum_of_the_markets() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let before = env.config().total_risk;
    assert_eq!(before, env.market(&m).risk);
    let v = env.view_market(&m);
    env.burn_direct(&alice, &m, 100 * USDC).unwrap();
    for i in 0..3u64 {
        // seen ahead of every burn: skipped.
        env.report(&m, i, v.burned_total + 100 * USDC + 1, v.minted_total).unwrap();
        books_hold(&env, &m);
        assert_eq!(env.config().total_risk, env.market(&m).risk, "after skipped report {i}");
    }
    let off = (100 * USDC) as u128 * 400_000;
    assert_eq!(env.market(&m).risk, before - off);
    assert_eq!(env.market(&m).backed, 0);
    env.refresh_risk(&[&m]).unwrap();
    assert_eq!((env.config().total_risk, env.market(&m).risk), (before - off, before - off));
}

/// Phase 7 review (high): the same market eight times in one batch, every
/// entry skipped, with a direct burn pending; then a batch mixing skipped
/// and applied entries for one market. Each entry sees what the one before
/// it stored, and the total stays the sum.
#[test]
fn duplicate_pairs_in_one_report_batch_keep_total_risk_the_sum() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.buy(&alice, &m, 600 * USDC, 600_000).unwrap();
    let v = env.view_market(&m);
    env.burn_direct(&alice, &m, 100 * USDC).unwrap();
    let bad = v.burned_total + 100 * USDC + 1;
    let before = env.config().total_risk;
    env.report_batch(&[(&m, 0, bad, v.minted_total); 8]).unwrap();
    books_hold(&env, &m);
    let mk = env.market(&m);
    assert_eq!(mk.risk, before - (100 * USDC) as u128 * 400_000);
    assert_eq!(env.config().total_risk, mk.risk);
    // Skipped, applied (hedge half), skipped, applied again (same holdings).
    env.burn_direct(&alice, &m, 10 * USDC).unwrap();
    let v = env.view_market(&m);
    let half = v.supply / 2;
    env.report_batch(&[(&m, 0, bad + 10 * USDC, v.minted_total), (&m, half, v.burned_total, v.minted_total), (&m, 0, v.burned_total, v.minted_total + 1), (&m, half, v.burned_total, v.minted_total)])
        .unwrap();
    books_hold(&env, &m);
    let mk = env.market(&m);
    assert_eq!(mk.backed, half);
    assert_eq!(mk.risk, (v.supply - half) as u128 * 400_000);
    assert_eq!(env.config().total_risk, mk.risk);
}

/// Phase 7 review: a stale read reported, then reported again, after the
/// supply dipped (the desk sold at the dip) and recovered with shares it
/// has not bought. Both reports are held to the dip.
#[test]
fn a_stale_read_reported_twice_around_a_dip_is_held_to_the_dip() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    let bob = env.user(10_000 * USDC);
    env.buy(&alice, &m, 100 * USDC, 500_000).unwrap();
    let v = env.view_market(&m);
    env.report(&m, v.supply, v.burned_total, v.minted_total).unwrap();
    // Read: the desk holds s. A third is redeemed (it sells as many), then
    // a buy brings the supply back up.
    let s = v.supply;
    let x = s / 3;
    env.sell(&alice, &m, x, 500_000).unwrap();
    env.buy(&bob, &m, 100 * USDC, 500_000).unwrap();
    let risk = env.market(&m).risk;
    env.report(&m, s, v.burned_total, v.minted_total).unwrap();
    assert_eq!((env.market(&m).backed, env.market(&m).risk), (s - x, risk));
    env.report(&m, s + 1, v.burned_total, v.minted_total).unwrap();
    assert_eq!((env.market(&m).backed, env.market(&m).risk), (s - x, risk));
    assert_eq!(env.config().total_risk, risk);
}

/// Phase 7 review: a direct burn of shares minted at 30c and at 60c since
/// the market was last fully backed takes (1 - 0.6) off each share, the
/// least any of them carried.
#[test]
fn a_direct_burn_takes_one_minus_high_off_each_share() {
    let (mut env, m) = setup();
    let alice = env.user(10_000 * USDC);
    env.buy(&alice, &m, 100 * USDC, 300_000).unwrap();
    env.buy(&alice, &m, 100 * USDC, 600_000).unwrap();
    let risk = env.market(&m).risk;
    env.burn_direct(&alice, &m, 10 * USDC).unwrap();
    env.refresh_risk(&[&m]).unwrap();
    assert_eq!(env.market(&m).risk, risk - (10 * USDC) as u128 * 400_000);
    assert_eq!(env.config().total_risk, env.market(&m).risk);
}
