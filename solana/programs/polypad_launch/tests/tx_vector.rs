//! Recorded transactions for the Solana indexer (services/solana/): a coin's
//! life run against the real Meteora programs, each successful transaction
//! kept as RPC `getTransaction` returns it (`encoding: "json"`, v0, inner
//! instructions with stack heights, token balances). The indexer's tests
//! decode these instead of hand-made bytes, so an event layout, an account
//! order or a Meteora event change shows up there.
//!
//! The run: a launch with the creator's first buy (the two-transaction
//! bundle), a second coin on the same market in one transaction, a buy and a
//! sale through the exchange, a plain coin transfer, buys that complete the
//! curve, graduation, a pool buy through the exchange, a pool sale with no
//! exchange, and a fee collect.
//!
//! Keys are fresh each run, so the fixture is rewritten only on purpose:
//! `WRITE_TX_FIXTURE=1 cargo test -p polypad_launch --test tx_vector`
//! writes `tests/fixtures/txs_v1.json` and prints it between
//! `TX_FIXTURE_BEGIN` and `TX_FIXTURE_END` (for a remote build). Needs the
//! Meteora binaries (`tests/fixtures/fetch.sh`).

mod common;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::spl_token;
use common::*;
use serde_json::{json, Value};
use solana_signer::Signer;

const P60: u64 = 600_000;
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/txs_v1.json");

/// Moves what `env` recorded since the last call into `out`, one label each.
fn take(env: &mut Env, out: &mut Vec<Value>, labels: &[&str]) {
    let got: Vec<Value> = env.recorded.drain(..).collect();
    assert_eq!(got.len(), labels.len(), "recorded {} transactions for {labels:?}", got.len());
    for (label, tx) in labels.iter().zip(got) {
        out.push(json!({ "label": label, "tx": tx }));
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn coin_json(c: &Coin) -> Value {
    json!({
        "mint": c.base_mint.to_string(),
        "positionIdHex": hex(&c.pid),
        "market": c.market.to_string(),
        "ptokenMint": c.ptoken.to_string(),
        "dbcConfig": c.config.to_string(),
        "pool": c.pool.to_string(),
        "dammPool": c.damm_pool().to_string(),
        "launch": c.launch.to_string(),
    })
}

#[test]
fn record_a_coins_life() {
    if std::env::var("WRITE_TX_FIXTURE").as_deref() != Ok("1") {
        // The fixture is only ever rewritten on purpose (fresh keys each run).
        return;
    }
    let mut env = Env::new();
    env.record = true;
    let mut out = vec![];

    let creator = env.user(20_000 * USDC);
    let a = LaunchArgs { first_buy: 50 * USDC, holders_bps: 2_500, ..Default::default() };
    let (c, _) = env.launch(&creator, pid(1), P60, &a).unwrap();
    take(&mut env, &mut out, &["launch with first buy, bundle tx A", "launch with first buy, bundle tx B"]);

    let creator2 = env.user(1_000 * USDC);
    let b = LaunchArgs { name: "Second".into(), symbol: "TWO".into(), uri: "https://polypad.fun/c/two.json".into(), ..Default::default() };
    let (c2, _) = env.launch(&creator2, pid(1), P60, &b).unwrap();
    take(&mut env, &mut out, &["launch without first buy, one transaction"]);

    env.warp(120);
    let u = env.user(1_000 * USDC);
    env.buy(&u, &c, 200 * USDC, P60).unwrap();
    take(&mut env, &mut out, &["buy on the curve"]);
    let coins = env.balance(&get_associated_token_address(&u.key(), &c.base_mint));
    env.sell(&u, &c, 100 * SHARE, coins, P60).unwrap();
    take(&mut env, &mut out, &["sell on the curve"]);

    // A plain transfer of coins between wallets: only holders change.
    let friend = env.user(0);
    let src = get_associated_token_address(&u.key(), &c.base_mint);
    let dst = env.ata(&friend.key(), &c.base_mint);
    let moved = env.balance(&src) / 4;
    let ix = spl_token::instruction::transfer_checked(&spl_token::ID, &src, &c.base_mint, &dst, &u.key(), &[], moved, 6).unwrap();
    let uk = u.kp.insecure_clone();
    env.send(&[ix], &uk, &[]).unwrap();
    take(&mut env, &mut out, &["coin transfer"]);

    env.warp(10);
    let whale = env.user(20_000 * USDC);
    env.buy(&whale, &c, 3_000 * USDC, P60).unwrap();
    env.buy(&whale, &c, 4_000 * USDC, P60).unwrap();
    take(&mut env, &mut out, &["buy on the curve (whale)", "buy that completes the curve"]);

    env.warp(10);
    env.migrate(&c).unwrap();
    take(&mut env, &mut out, &["graduate"]);

    env.warp(10);
    let q = env.quote(c.pid, BUY, P60, 1_000 * USDC);
    let shares = env.minted(P60, 1_000 * USDC);
    let ixs = vec![env.ed25519_ix(&q), env.mint_ix(&whale, &c, 1_000 * USDC, &q), env.pool_swap_ix(&whale.key(), &c, true, shares)];
    let w = whale.kp.insecure_clone();
    env.send_v0(&ixs, &w, &[]).unwrap();
    take(&mut env, &mut out, &["buy on the pool"]);

    let held = env.balance(&get_associated_token_address(&whale.key(), &c.base_mint));
    let ix = env.pool_swap_ix(&whale.key(), &c, false, held / 10);
    env.send_v0(&[ix], &w, &[]).unwrap();
    take(&mut env, &mut out, &["sell on the pool, no exchange"]);

    env.collect(&c).unwrap();
    take(&mut env, &mut out, &["collect"]);

    let v = json!({
        "about": "Recorded by solana/programs/polypad_launch/tests/tx_vector.rs (LiteSVM, mainnet Meteora binaries), read by services/solana/core.test.ts.",
        "exchangeProgram": px::ID.to_string(),
        "launchProgram": pl::ID.to_string(),
        "usdcMint": env.usdc_mint.to_string(),
        "signer": env.signer_key().to_string(),
        "platform": env.platform.pubkey().to_string(),
        "feeVault": env.fee_vault().to_string(),
        "creator": creator.key().to_string(),
        "creator2": creator2.key().to_string(),
        "trader": u.key().to_string(),
        "friend": friend.key().to_string(),
        "whale": whale.key().to_string(),
        "lookupTable": env.alt.as_ref().map(|a| a.key.to_string()),
        "coins": [coin_json(&c), coin_json(&c2)],
        "transactions": out,
    });
    let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
    std::fs::write(FIXTURE, &text).unwrap();
    println!("TX_FIXTURE_BEGIN\n{text}TX_FIXTURE_END");
}
