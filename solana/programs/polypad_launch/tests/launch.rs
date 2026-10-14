//! `polypad_launch` against the real Meteora programs: launch, the creator's
//! first buy, the anti-sniper schedule, buys and sells through the exchange in
//! one transaction, curve completion, migration to DAMM v2 with the LP
//! locked, fee claims and their split, payees, and hijack attempts.
//!
//! Compute units and transaction sizes are printed with `--nocapture`.

mod common;
use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::{system_program, InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address;
use common::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const P60: u64 = 600_000;

fn report(what: &str, s: &Sent) {
    eprintln!("{what}: {} CU, {} bytes (limit 1232), {} accounts; per instruction {:?}", s.cu, s.size, s.accounts, s.per_instruction());
}

/// Trading fee the curve charged on the last swap, from the pool's counters.
fn fees_of(env: &Env, pool: &Pubkey) -> u64 {
    let p = env.curve_pool(pool).pool_state;
    p.partner_quote_fee + p.protocol_quote_fee + p.creator_quote_fee
}

fn launched(env: &mut Env, id: u8, a: &LaunchArgs) -> (User, Coin) {
    let creator = env.user(20_000 * USDC);
    let (c, _) = env.launch(&creator, pid(id), P60, a).unwrap();
    (creator, c)
}

/* ------------------------------------------------------------------ launch */

#[test]
fn launch_creates_the_coin_on_its_own_config() {
    let mut env = Env::new();
    let creator = env.user(1_000 * USDC);
    let (c, sent) = env.launch(&creator, pid(1), P60, &LaunchArgs { holders_bps: 2_500, ..Default::default() }).unwrap();
    report("launch: ed25519 + launch + DBC pool creation (v0 + lookup table)", &sent[0]);
    assert!(sent[0].size <= 1232);

    let l = env.launch_of(&c);
    assert_eq!(l.base_mint, c.base_mint);
    assert_eq!(l.ptoken_mint, c.ptoken);
    assert_eq!((l.dbc_config, l.pool), (c.config, c.pool));
    assert_eq!((l.creator, l.payee), (creator.key(), creator.key()));
    assert_eq!(l.holders_bps, 2_500);
    assert_eq!((l.price, l.phantom, l.threshold), (P60, 4_000_000_000, 10_000_000_000));
    assert_eq!(l.fees, Fees::DEFAULT);
    assert!(!l.graduated);

    // DBC computed exactly the doc's numbers, with our pToken and our vault.
    let p = env.pool_config(&c);
    assert_eq!(p.quote_mint, c.ptoken);
    assert_eq!((p.fee_claimer, p.leftover_receiver), (env.fee_vault(), env.fee_vault()));
    assert_eq!(p.migration_quote_threshold, 10_000_000_000);
    assert_eq!(p.swap_base_amount, 714_285_714_285_715);
    assert_eq!(p.migration_base_threshold, 204_081_639_440_922);
    assert_eq!(p.migration_sqrt_price, 129_127_208_515_966_861);
    assert_eq!(p.post_migration_token_supply, 918_367_353_726_637);
    assert_eq!(p.partner_permanent_locked_liquidity_percentage, 100);
    assert_eq!(p.pool_fees.base_fee.cliff_fee_numerator, 254_000_000);

    // The coin: fixed supply in the curve, no mint or freeze authority.
    let pool = env.curve_pool(&c.pool).pool_state;
    assert_eq!((pool.creator, pool.base_mint, pool.config), (creator.key(), c.base_mint, c.config));
    assert_eq!(env.supply(&c.base_mint), 1_000_000_000_000_000);
    assert_eq!(env.balance(&c.base_vault), 1_000_000_000_000_000);
    let mint = anchor_spl::token::spl_token::state::Mint::unpack_from_slice(&env.svm.get_account(&c.base_mint).unwrap().data).unwrap();
    assert!(mint.mint_authority.is_none() && mint.freeze_authority.is_none());
    assert!(env.exists(&pda(&[b"metadata", METADATA_PROGRAM.as_ref(), c.base_mint.as_ref()], &METADATA_PROGRAM)));
    // The market and its pToken came into being with the coin.
    assert!(env.exists(&c.market) && env.exists(&c.ptoken));
}

#[test]
fn creator_first_buy_pays_the_curve_fee_in_the_launch_bundle() {
    let mut env = Env::new();
    let creator = env.user(1_000 * USDC);
    let a = LaunchArgs { first_buy: 100 * USDC, ..Default::default() };
    let (c, sent) = env.launch(&creator, pid(1), P60, &a).unwrap();
    report("bundle tx A: ed25519 + launch with the first-buy mint", &sent[0]);
    report("bundle tx B: DBC pool creation + ATA + swap2 (first swap)", &sent[1]);

    let shares = env.minted(P60, 100 * USDC);
    assert_eq!(env.balance(&creator.usdc), 900 * USDC);
    // All the minted pTokens went into the curve, at 1.4%, not the schedule's 25.4%.
    assert_eq!(env.balance(&get_associated_token_address(&creator.key(), &c.ptoken)), 0);
    let fee = fees_of(&env, &c.pool);
    // DBC rounds fees up.
    assert_eq!(fee, (shares as u128 * 14).div_ceil(1_000) as u64);
    assert!(env.balance(&get_associated_token_address(&creator.key(), &c.base_mint)) > 0);
}

#[test]
fn snipers_pay_the_schedule_for_a_minute() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    let sniper = env.user(10_000 * USDC);
    let mut last = 0;
    for (wait, want_bps) in [(0i64, 2_540u64), (30, 1_340), (30, 140), (600, 140)] {
        env.warp(wait);
        let shares = env.minted(P60, 10 * USDC);
        env.buy(&sniper, &c, 10 * USDC, P60).unwrap();
        let fee = fees_of(&env, &c.pool) - last;
        last += fee;
        assert_eq!(fee, (shares as u128 * want_bps as u128).div_ceil(10_000) as u64, "after {wait}s");
    }
}

#[test]
fn launch_refuses_bad_quotes() {
    let mut env = Env::new();
    let creator = env.user(1_000 * USDC);
    let id = pid(1);
    let try_launch = |env: &mut Env, q: Quote, sign_with: Option<ed25519_dalek::SigningKey>| {
        let mint = Keypair::new();
        let c = env.coin(id, mint.pubkey());
        let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
        if let Some(k) = sign_with {
            ixs[0] = Env::ed25519_ix_by(&k, &q.message(&px::ID));
        }
        let kp = creator.kp.insecure_clone();
        env.send_v0(&ixs, &kp, &[&mint])
    };
    let q = env.quote(id, BUY, P60, 1);
    let now = env.now;
    expect_err(try_launch(&mut env, Quote { valid_until: now - 1, ..q }, None), LaunchError::QuoteExpired);
    expect_err(try_launch(&mut env, Quote { valid_until: now + 31, ..q }, None), LaunchError::QuoteTooLong);
    expect_err(try_launch(&mut env, Quote { side: SELL, ..q }, None), LaunchError::WrongSide);
    expect_err(try_launch(&mut env, q, Some(ed25519_dalek::SigningKey::from_bytes(&[7; 32]))), LaunchError::BadSignature);
    // Outside the exchange's band.
    expect_err(try_launch(&mut env, Quote { price: 999_500, ..q }, None), LaunchError::PriceOutOfRange);
    env.ex_owner_ix(px::instruction::SetParams {
        buy_spread_bps: 25,
        sell_spread_bps: 25,
        min_price: 100_000,
        max_price: 900_000,
        default_max_risk: u64::MAX,
    })
    .unwrap();
    expect_err(try_launch(&mut env, Quote { price: 50_000, ..q }, None), LaunchError::PriceOutOfBand);
    expect_err(try_launch(&mut env, Quote { price: 950_000, ..q }, None), LaunchError::PriceOutOfBand);
    env.ex_owner_ix(px::instruction::SetParams {
        buy_spread_bps: 25,
        sell_spread_bps: 25,
        min_price: 1_000,
        max_price: 999_000,
        default_max_risk: u64::MAX,
    })
    .unwrap();

    // No ed25519 instruction at all.
    let mint = Keypair::new();
    let c = env.coin(id, mint.pubkey());
    let ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    let kp = creator.kp.insecure_clone();
    expect_err(env.send_v0(&ixs[1..], &kp, &[&mint]), LaunchError::NotEd25519);

    // A paused market, then a settled one.
    let ok = env.launch(&creator, id, P60, &LaunchArgs::default());
    assert!(ok.is_ok());
    let m = env.coin(id, Pubkey::default()).market;
    let keeper = env.keeper.insecure_clone();
    let pause = Instruction {
        program_id: px::ID,
        accounts: {
            let mut v = px::accounts::KeeperBatch {
                keeper: keeper.pubkey(),
                config: env.ex_config(),
                event_authority: env.ex_events(),
                program: px::ID,
            }
            .to_account_metas(None);
            v.push(anchor_lang::solana_program::instruction::AccountMeta::new(m, false));
            v
        },
        data: px::instruction::SetPaused { paused: true }.data(),
    };
    env.send(&[pause], &keeper, &[]).unwrap();
    let q = env.quote(id, BUY, P60, 1);
    expect_err(try_launch(&mut env, q, None), LaunchError::MarketPaused);
    let settle = Instruction {
        program_id: px::ID,
        accounts: px::accounts::MarketAdmin {
            authority: keeper.pubkey(),
            config: env.ex_config(),
            market: m,
            event_authority: env.ex_events(),
            program: px::ID,
        }
        .to_account_metas(None),
        data: px::instruction::Settle { payout: 1_000_000 }.data(),
    };
    env.send(&[settle], &keeper, &[]).unwrap();
    env.warp(3_601);
    let q = env.quote(id, BUY, P60, 1);
    expect_err(try_launch(&mut env, q, None), LaunchError::MarketSettled);
}

#[test]
fn only_the_mints_holder_can_open_the_pool_launch_recorded() {
    let mut env = Env::new();
    let creator = env.user(1_000 * USDC);
    let attacker = env.user(1_000 * USDC);
    let id = pid(1);
    let kp = creator.kp.insecure_clone();
    let ak = attacker.kp.insecure_clone();

    // The mint must sign: nobody can make a config for someone else's mint.
    let mint = Keypair::new();
    let c = env.coin(id, mint.pubkey());
    let q = env.quote(id, BUY, P60, 1);
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    ixs.truncate(2);
    for m in ixs[1].accounts.iter_mut().filter(|m| m.pubkey == c.base_mint) {
        m.is_signer = false;
    }
    expect_custom(env.send_v0(&ixs, &kp, &[]), 2, 3010); // AccountNotSigner

    // Holders share above 100%.
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs { holders_bps: 10_001, ..Default::default() }).remove(0);
    ixs.truncate(2);
    expect_err(env.send_v0(&ixs, &kp, &[&mint]), LaunchError::BadHoldersShare);

    // Launch alone (the first transaction of a bundle that lost its second):
    // the record names a pool that does not exist yet.
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    ixs.truncate(2);
    env.send_v0(&ixs, &kp, &[&mint]).unwrap();
    assert!(env.exists(&c.launch) && env.exists(&c.config) && !env.exists(&c.pool));

    // Nobody else can open it: the pool's address needs this mint, and the
    // mint's keypair must sign DBC's pool creation.
    let init = env.pool_init_ix(&ak.pubkey(), &ak.pubkey(), &c, &LaunchArgs::default());
    let mut forged = init.clone();
    for m in forged.accounts.iter_mut().filter(|m| m.pubkey == c.base_mint) {
        m.is_signer = false;
    }
    assert!(env.send_v0(&[forged], &ak, &[]).is_err());
    // A pool on this config with another mint is another pool (see the hijack test).

    // The creator opens it a few seconds later, before the quote expires
    // (a bundle that landed late); their buy is still the first and pays
    // the curve fee. After the quote's expiry it would not be the coin
    // (a_pool_opened_after_the_quote_expired_is_not_the_coin).
    env.warp(10);
    let p = env.ata(&creator.key(), &c.ptoken);
    env.set_token(p, c.ptoken, creator.key(), 100 * SHARE);
    let ixs = vec![
        env.pool_init_ix(&creator.key(), &creator.key(), &c, &LaunchArgs::default()),
        Env::ata_create_ix(&creator.key(), &creator.key(), &c.base_mint),
        env.curve_swap_ix(&creator.key(), &c, true, 100 * SHARE, 0, 1, true),
    ];
    env.send_v0(&ixs, &kp, &[&mint]).unwrap();
    assert_eq!(fees_of(&env, &c.pool), (100 * SHARE as u128 * 14).div_ceil(1_000) as u64);
}

/// The worst-case strings (Metaplex maxima) and a typical IPFS URI.
#[test]
fn launch_transactions_fit_with_a_lookup_table() {
    let mut env = Env::new();
    let creator = env.user(1_000 * USDC);
    for (label, uri) in [
        ("31 B uri", "https://polypad.fun/c/rain.json".to_string()),
        ("67 B uri", format!("https://ipfs.io/ipfs/{}", "Q".repeat(46))),
        ("200 B uri", "u".repeat(200)),
    ] {
        let name = "Will it rain in London tomorrow".to_string();
        for first_buy in [0, 50 * USDC] {
            let a = LaunchArgs { uri: uri.clone(), name: name.clone(), symbol: "RAINLONDON".into(), first_buy, holders_bps: 0 };
            let mint = Keypair::new();
            let c = env.coin(pid(1), mint.pubkey());
            let q = env.quote(pid(1), BUY, P60, first_buy.max(1));
            let txs = env.launch_txs(&creator, &c, &q, &a);
            let kp = creator.kp.insecure_clone();
            if first_buy == 0 {
                let s = env.send_v0(&txs[0], &kp, &[&mint]).unwrap();
                eprintln!("{label}, one tx: {} bytes, {} CU", s.size, s.cu);
                if s.size > 1232 {
                    eprintln!("  too big: send it as the bundle below instead");
                }
                continue;
            }
            let a1 = env.send_v0(&txs[0], &kp, &[&mint]).unwrap();
            let b1 = env.send_v0(&txs[1], &kp, &[&mint]).unwrap();
            eprintln!("{label}, bundle: tx A {} bytes {} CU, tx B {} bytes {} CU", a1.size, a1.cu, b1.size, b1.cu);
            assert!(a1.size <= 1232 && b1.size <= 1232);
        }
    }
}

/* ---------------------------------------------------------------- hijacks */

#[test]
fn quote_mint_must_be_the_markets_ptoken() {
    let mut env = Env::new();
    let creator = env.user(1_000 * USDC);
    let (_, other_market) = launched(&mut env, 2, &LaunchArgs::default());
    let kp = creator.kp.insecure_clone();
    let mint = Keypair::new();
    let c = env.coin(pid(1), mint.pubkey());
    let q = env.quote(pid(1), BUY, P60, 1);

    // The launch instruction given another market's pToken: refused by its seeds.
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    for m in ixs[1].accounts.iter_mut().filter(|m| m.pubkey == c.ptoken) {
        m.pubkey = other_market.ptoken;
    }
    expect_custom(env.send_v0(&ixs, &kp, &[&mint]), 2, 2006); // ConstraintSeeds

    // A pool on the coin's config with another quote mint (its own pool and
    // vault addresses): DBC's `has_one = quote_mint` on the config refuses it.
    let mut wrong = Coin { ptoken: other_market.ptoken, ..c.clone() };
    wrong.pool = pool_address(&c.config, &c.base_mint, &wrong.ptoken);
    wrong.base_vault = pda(&[b"token_vault", c.base_mint.as_ref(), wrong.pool.as_ref()], &DBC_ID);
    wrong.quote_vault = pda(&[b"token_vault", wrong.ptoken.as_ref(), wrong.pool.as_ref()], &DBC_ID);
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    ixs[2] = env.pool_init_ix(&creator.key(), &creator.key(), &wrong, &LaunchArgs::default());
    expect_custom(env.send_v0(&ixs, &kp, &[&mint]), 3, 2001); // ConstraintHasOne
}

#[test]
fn foreign_configs_and_extra_pools_never_reach_the_fee_vault_books() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    let attacker = env.user(1_000 * USDC);
    let ak = attacker.kp.insecure_clone();
    let a = LaunchArgs { name: "Will it rain (real)".into(), ..Default::default() };

    // 1. A look-alike config anyone can make: our pToken, our vault as fee
    //    claimer, any curve. DBC allows it.
    let fake_config = Keypair::new();
    let fake_params = config_parameters(&pl::curve::curve(6_000_000_000, 10_000).unwrap(), &Fees::DEFAULT);
    let create = Instruction {
        program_id: DBC_ID,
        accounts: dbc::client::accounts::CreateConfig {
            config: fake_config.pubkey(),
            fee_claimer: env.fee_vault(),
            leftover_receiver: env.fee_vault(),
            quote_mint: c.ptoken,
            payer: ak.pubkey(),
            system_program: system_program::ID,
            event_authority: DBC_EVENT_AUTHORITY,
            program: DBC_ID,
        }
        .to_account_metas(None),
        data: dbc::client::args::CreateConfig { config_parameters: fake_params }.data(),
    };
    let fake_mint = Keypair::new();
    let mut f = env.coin(c.pid, fake_mint.pubkey());
    f.config = fake_config.pubkey();
    f.pool = pool_address(&f.config, &f.base_mint, &f.ptoken);
    f.base_vault = pda(&[b"token_vault", f.base_mint.as_ref(), f.pool.as_ref()], &DBC_ID);
    f.quote_vault = pda(&[b"token_vault", f.ptoken.as_ref(), f.pool.as_ref()], &DBC_ID);
    let init = env.pool_init_ix(&ak.pubkey(), &ak.pubkey(), &f, &a);
    env.send(&[create, init], &ak, &[&fake_config, &fake_mint]).unwrap();
    // It has no Launch record and cannot get one: `launch` only signs for its own PDA.
    assert!(!env.exists(&f.launch));
    let q = env.quote(c.pid, BUY, P60, 1);
    let m2 = Keypair::new();
    let mut ixs = env.launch_txs(&attacker, &env.coin(c.pid, m2.pubkey()), &q, &a).remove(0);
    for m in ixs[1].accounts.iter_mut().filter(|m| m.pubkey == env.coin(c.pid, m2.pubkey()).config) {
        m.pubkey = fake_config.pubkey();
    }
    expect_custom(env.send_v0(&ixs, &ak, &[&m2]), 2, 2006);

    // 2. A second pool on our own per-coin config, with a fresh mint: DBC
    //    allows it too (configs are reusable by anyone).
    let extra_mint = Keypair::new();
    let mut e = env.coin(c.pid, extra_mint.pubkey());
    e.config = c.config;
    e.pool = pool_address(&e.config, &e.base_mint, &e.ptoken);
    e.base_vault = pda(&[b"token_vault", e.base_mint.as_ref(), e.pool.as_ref()], &DBC_ID);
    e.quote_vault = pda(&[b"token_vault", e.ptoken.as_ref(), e.pool.as_ref()], &DBC_ID);
    let init = env.pool_init_ix(&ak.pubkey(), &ak.pubkey(), &e, &a);
    env.send(&[init], &ak, &[&extra_mint]).unwrap();
    assert!(!env.exists(&e.launch));
    // Trades on it pay fees to our vault's DBC account for that pool...
    env.warp(120);
    env.buy(&attacker, &e, 100 * USDC, P60).unwrap();
    env.buy(&attacker, &f, 100 * USDC, P60).unwrap();

    // ...but `collect` claims only the pool on the coin's record.
    let k = env.keeper.insecure_clone();
    for (pool, base_vault, quote_vault) in [(e.pool, e.base_vault, e.quote_vault), (f.pool, f.base_vault, f.quote_vault)] {
        let mut ix = env.collect_ix(&k.pubkey(), &c);
        for m in ix.accounts.iter_mut() {
            if m.pubkey == c.pool {
                m.pubkey = pool;
            } else if m.pubkey == c.base_vault {
                m.pubkey = base_vault;
            } else if m.pubkey == c.quote_vault {
                m.pubkey = quote_vault;
            }
        }
        expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::WrongAccount);
    }
    let mut ix = env.collect_ix(&k.pubkey(), &c);
    for m in ix.accounts.iter_mut().filter(|m| m.pubkey == c.config) {
        m.pubkey = fake_config.pubkey();
    }
    expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::WrongAccount);
    // The real coin's collect books nothing from them.
    env.collect(&c).unwrap();
    assert_eq!(env.launch_of(&c).curve_fees, 0);

    // The keeper sweeps their partner fees to the platform instead.
    let platform = env.platform.pubkey();
    let mut swept = 0;
    for (config, pool, base) in [(c.config, e.pool, e.base_mint), (fake_config.pubkey(), f.pool, f.base_mint)] {
        let partner = env.curve_pool(&pool).pool_state.partner_quote_fee;
        assert!(partner > 0);
        expect_err(env.send_v0(&[env.sweep_ix(&ak.pubkey(), &config, &pool, &base, &c.ptoken)], &ak, &[]), LaunchError::OnlyKeeper);
        let sent = env.send_v0(&[env.sweep_ix(&k.pubkey(), &config, &pool, &base, &c.ptoken)], &k, &[]).unwrap();
        report("sweep_fees (look-alike pool)", &sent);
        swept += partner;
        assert_eq!(env.platform_owed(&c.ptoken, &platform), swept);
        assert_eq!(env.curve_pool(&pool).pool_state.partner_quote_fee, 0);
    }
    // Never a coin's own pool: its fees go through the split.
    expect_err(env.send_v0(&[env.sweep_ix(&k.pubkey(), &c.config, &c.pool, &c.base_mint, &c.ptoken)], &k, &[]), LaunchError::IsCoin);
    let pk = env.platform.insecure_clone();
    let to = env.ata(&platform, &c.ptoken);
    env.withdraw_as(&pk, &c.ptoken, &to, true).unwrap();
    assert_eq!(env.balance(&to), swept);
    assert_eq!(env.balance(&get_associated_token_address(&env.fee_vault(), &c.ptoken)), 0);
}

/// A look-alike config may take a live coin as its quote mint. Its fees
/// would land in the vault's account for that coin, which `burn_leftover`
/// burns, so `sweep_fees` refuses it as it refuses a coin's own pool:
/// nothing is ever owed in a coin's mint.
#[test]
fn a_look_alike_quoted_in_a_coin_is_never_swept() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    let attacker = env.user(0);
    let ak = attacker.kp.insecure_clone();
    let fake_config = Keypair::new();
    let params = config_parameters(&pl::curve::curve(6_000 * USDC, P60).unwrap(), &Fees::DEFAULT);
    let create = Instruction {
        program_id: DBC_ID,
        accounts: dbc::client::accounts::CreateConfig {
            config: fake_config.pubkey(),
            fee_claimer: env.fee_vault(),
            leftover_receiver: env.fee_vault(),
            quote_mint: c.base_mint,
            payer: ak.pubkey(),
            system_program: system_program::ID,
            event_authority: DBC_EVENT_AUTHORITY,
            program: DBC_ID,
        }
        .to_account_metas(None),
        data: dbc::client::args::CreateConfig { config_parameters: params }.data(),
    };
    let m = Keypair::new();
    let mut f = env.coin(c.pid, m.pubkey());
    f.ptoken = c.base_mint;
    f.config = fake_config.pubkey();
    f.pool = pool_address(&f.config, &f.base_mint, &f.ptoken);
    f.base_vault = pda(&[b"token_vault", f.base_mint.as_ref(), f.pool.as_ref()], &DBC_ID);
    f.quote_vault = pda(&[b"token_vault", f.ptoken.as_ref(), f.pool.as_ref()], &DBC_ID);
    let init = env.pool_init_ix(&ak.pubkey(), &ak.pubkey(), &f, &LaunchArgs::default());
    env.send(&[create, init], &ak, &[&fake_config, &m]).unwrap();
    // Trades on it pay our vault partner fees in the coin.
    env.warp(120);
    env.set_token(get_associated_token_address(&ak.pubkey(), &c.base_mint), c.base_mint, ak.pubkey(), 1_000 * SHARE);
    env.ata(&ak.pubkey(), &f.base_mint);
    let ix = env.curve_swap_ix(&ak.pubkey(), &f, true, 1_000 * SHARE, 0, 1, false);
    env.send_v0(&[ix], &ak, &[]).unwrap();
    assert!(env.curve_pool(&f.pool).pool_state.partner_quote_fee > 0);

    let k = env.keeper.insecure_clone();
    let ix = env.sweep_ix(&k.pubkey(), &f.config, &f.pool, &f.base_mint, &c.base_mint);
    expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::IsCoin);
    assert_eq!(env.platform_owed(&c.base_mint, &env.platform.pubkey()), 0);
}

/* ----------------------------------------------------------------- trading */

#[test]
fn buys_and_sells_go_through_the_exchange_in_one_transaction() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    env.warp(120);
    let u = env.user(1_000 * USDC);
    let sent = env.buy(&u, &c, 200 * USDC, P60).unwrap();
    report("buy: ed25519 + exchange mint + DBC swap2", &sent);
    assert!(sent.size <= 1232);
    let coins = env.balance(&get_associated_token_address(&u.key(), &c.base_mint));
    assert!(coins > 0);
    assert_eq!(env.balance(&u.usdc), 800 * USDC);
    assert_eq!(env.balance(&get_associated_token_address(&u.key(), &c.ptoken)), 0);

    // Sell enough coins for exactly 100 shares, redeemed for USDC at once.
    let sent = env.sell(&u, &c, 100 * SHARE, coins, P60).unwrap();
    report("sell: DBC swap2 (exact out) + ed25519 + exchange redeem", &sent);
    assert!(sent.size <= 1232);
    let usdc = env.balance(&u.usdc) - 800 * USDC;
    assert_eq!(usdc, (100 * SHARE as u128 * (P60 as u128 * 9_975 / 10_000) / 1_000_000) as u64);
    assert!(env.balance(&get_associated_token_address(&u.key(), &c.base_mint)) < coins);
}

/// Buys until the curve completes; returns the buyer.
fn complete(env: &mut Env, c: &Coin) -> User {
    let whale = env.user(20_000 * USDC);
    env.warp(120);
    env.buy(&whale, c, 3_000 * USDC, P60).unwrap();
    env.buy(&whale, c, 4_000 * USDC, P60).unwrap();
    whale
}

#[test]
fn the_curve_completes_at_its_threshold_and_migrates_with_the_lp_locked() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    // Graduation before the curve is done: nothing to migrate or record.
    let k = env.keeper.insecure_clone();
    let (m1, m2) = (Keypair::new(), Keypair::new());
    let ix = env.graduate_ix(&k.pubkey(), &c, &m1.pubkey(), Some(&m2.pubkey()));
    expect_err(env.send_v0(&[ix], &k, &[&m1, &m2]), LaunchError::NotMigrated);

    let whale = complete(&mut env, &c);
    let pool = env.curve_pool(&c.pool).pool_state;
    // PartialFill stops at the migration price: q* plus one atom of rounding
    // (DBC rounds the quote in up), which is the only surplus.
    assert_eq!(pool.quote_reserve, 10_000_000_001);
    // The buy that crossed kept its unused pTokens.
    assert!(env.balance(&get_associated_token_address(&whale.key(), &c.ptoken)) > 0);
    assert_eq!(pool.migration_progress, 2);
    // Completed but not migrated: every swap is refused (unlike the EVM).
    assert!(env.buy(&whale, &c, 10 * USDC, P60).is_err());

    let (sent, nft) = env.migrate(&c).unwrap();
    report("graduate: migration_damm_v2 by CPI through Hv8L + record (v0 + lookup table)", &sent);
    assert!(sent.size <= 1232);
    let l = env.launch_of(&c);
    assert!(l.graduated);
    assert_eq!((l.damm_config, l.damm_pool), (DAMM_CONFIG, c.damm_pool()));
    let pos = env.position_state(&l.position);
    let dp = env.damm_pool_state(&c);
    assert_eq!(pos.unlocked_liquidity, 0);
    assert_eq!(pos.permanent_locked_liquidity, dp.permanent_lock_liquidity);
    // What `graduate` expects, fixed at launch, is what DBC put there.
    assert_eq!(l.locked_liquidity, pos.permanent_locked_liquidity);
    eprintln!(
        "pool liquidity {} locked {} (dust {})",
        dp.liquidity,
        pos.permanent_locked_liquidity,
        dp.liquidity - pos.permanent_locked_liquidity
    );
    assert!(env.balance(&l.position_nft_account) == 1);
    let nft_owner = Pubkey::new_from_array(env.svm.get_account(&l.position_nft_account).unwrap().data[32..64].try_into().unwrap());
    assert_eq!(nft_owner, env.fee_vault());
    assert_eq!(l.position_nft_account, pda(&[b"position_nft_account", nft.as_ref()], &DAMM_ID));
    // The pool opened at the curve's last price with q* less Meteora's 0.2%.
    assert_eq!(dp.sqrt_price, 129_127_208_515_966_861);
    assert_eq!(dp.token_b_amount, 9_980_000_000);
    // The unsold remainder burned for real: supply is the post-migration supply.
    assert_eq!(env.supply(&c.base_mint), 918_367_353_726_637);
    // Graduating twice is refused.
    let ix = env.graduate_ix(&k.pubkey(), &c, &nft, None);
    expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::AlreadyGraduated);

    // The graduated pool trades.
    let p = get_associated_token_address(&whale.key(), &c.ptoken);
    let before = env.balance(&get_associated_token_address(&whale.key(), &c.base_mint));
    let ix = env.pool_swap_ix(&whale.key(), &c, true, env.balance(&p));
    let w = whale.kp.insecure_clone();
    env.send_v0(&[ix], &w, &[]).unwrap();
    assert!(env.balance(&get_associated_token_address(&whale.key(), &c.base_mint)) > before);
}

#[test]
fn graduate_checks_the_position() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    complete(&mut env, &c);
    let k = env.keeper.insecure_clone();
    let (ix, m1, m2) = env.migrate_ix(&k.pubkey(), &c);
    // DBC's migration alone, as a legacy transaction (no lookup table).
    let sent = env.send(&[ix], &k, &[&m1, &m2]).unwrap();
    report("DBC migration_damm_v2 alone, legacy", &sent);
    // The second (dust) position belongs to the creator and is unlocked: not ours.
    let ix = env.graduate_ix(&k.pubkey(), &c, &m2.pubkey(), None);
    let r = env.send_v0(&[ix], &k, &[]);
    assert!(r.is_err(), "second position accepted: {r:?}");
    // The keeper records the real one later.
    let ix = env.graduate_ix(&k.pubkey(), &c, &m1.pubkey(), None);
    let sent = env.send_v0(&[ix], &k, &[]).unwrap();
    report("graduate after someone else migrated (records only)", &sent);
}

/// Anyone may migrate, and anyone may add liquidity to the pool before
/// `graduate` lands: neither can stop the coin graduating, and positions
/// others make (for themselves or for our vault) are never taken for ours.
#[test]
fn third_party_liquidity_cannot_block_graduation() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    let whale = complete(&mut env, &c);
    // The keeper's graduation, signed but not yet landed.
    let k = env.keeper.insecure_clone();
    let (k1, k2) = (Keypair::new(), Keypair::new());
    let keeper_tx = env.graduate_ix(&k.pubkey(), &c, &k1.pubkey(), Some(&k2.pubkey()));

    // The attacker migrates first (DBC allows anyone), then adds liquidity
    // to the new pool from coins and pTokens bought on it.
    let attacker = env.user(0);
    let ak = attacker.kp.insecure_clone();
    let (ix, m1, m2) = env.migrate_ix(&ak.pubkey(), &c);
    env.send(&[ix], &ak, &[&m1, &m2]).unwrap();
    let w = whale.kp.insecure_clone();
    let wp = get_associated_token_address(&whale.key(), &c.ptoken);
    let ix = env.pool_swap_ix(&whale.key(), &c, true, env.balance(&wp) / 2);
    env.send_v0(&[ix], &w, &[]).unwrap();
    let (open, nft) = env.open_position_ix(&whale.key(), &whale.key(), &c);
    let liquidity = env.damm_pool_state(&c).liquidity / 1_000;
    let add = env.add_liquidity_ix(&whale.key(), &c, &nft.pubkey(), liquidity);
    // ...and locks it for good, so it looks like ours but for its owner.
    let lock = env.lock_position_ix(&whale.key(), &c, &nft.pubkey(), liquidity);
    env.send_v0(&[open, add, lock], &w, &[&nft]).unwrap();
    // A position for our vault that it never funded.
    let (open_v, nft_v) = env.open_position_ix(&whale.key(), &env.fee_vault(), &c);
    env.send_v0(&[open_v], &w, &[&nft_v]).unwrap();
    let ours = env.position_state(&pda(&[b"position", m1.pubkey().as_ref()], &DAMM_ID)).permanent_locked_liquidity;
    // Far above the old 1 ppm tolerance against the pool's live total.
    assert!(env.damm_pool_state(&c).liquidity > ours + ours / 1_000_000);

    // The keeper's migrating call now fails on its own accounts...
    assert!(env.send_v0(&[keeper_tx], &k, &[&k1, &k2]).is_err());
    // ...neither look-alike position is taken for the vault's...
    for fake in [nft.pubkey(), nft_v.pubkey()] {
        let ix = env.graduate_ix(&k.pubkey(), &c, &fake, None);
        expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::NotLocked);
    }
    // ...and the resend with the position DBC made graduates the coin.
    let ix = env.graduate_ix(&k.pubkey(), &c, &m1.pubkey(), None);
    env.send_v0(&[ix], &k, &[]).unwrap();
    let l = env.launch_of(&c);
    assert!(l.graduated);
    assert_eq!((l.damm_config, l.damm_pool, l.locked_liquidity), (DAMM_CONFIG, c.damm_pool(), ours));
    assert_eq!(l.position, pda(&[b"position", m1.pubkey().as_ref()], &DAMM_ID));
    // Fees from the pool reach the vault's books.
    let ix = env.pool_swap_ix(&whale.key(), &c, true, env.balance(&wp));
    env.send_v0(&[ix], &w, &[]).unwrap();
    env.collect(&c).unwrap();
    assert!(env.launch_of(&c).pool_fees > 0);
}

/// Whoever migrates first can fund and lock a position with exactly the
/// liquidity DBC's holds and hand its NFT account to our vault (in the
/// migration's own transaction even): on chain it is the twin of DBC's.
/// Only the keeper, who takes the first position from DBC's migration
/// transaction, may record a migration `graduate` did not run, so the twin
/// is never recorded and the real position's fees are never stranded.
#[test]
fn a_twin_position_handed_to_the_vault_is_never_recorded() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    complete(&mut env, &c);
    let attacker = env.user(0);
    let ak = attacker.kp.insecure_clone();
    let (ix, m1, m2) = env.migrate_ix(&ak.pubkey(), &c);
    env.send(&[ix], &ak, &[&m1, &m2]).unwrap();
    let real = m1.pubkey();
    let ours = env.position_state(&pda(&[b"position", real.as_ref()], &DAMM_ID)).permanent_locked_liquidity;
    assert_eq!(ours, pl::curve::migrated_liquidity(env.launch_of(&c).threshold, env.pool_config(&c).migration_sqrt_price, env.curve_pool(&c.pool).pool_state.protocol_liquidity_migration_fee_bps).unwrap());

    let twin = env.locked_position(&ak, &c, ours, true).unwrap();
    let t = env.position_state(&pda(&[b"position", twin.as_ref()], &DAMM_ID));
    assert_eq!((t.unlocked_liquidity, t.vested_liquidity, t.permanent_locked_liquidity), (0, 0, ours));
    assert_eq!((env.nft_owner(&twin), env.nft_owner(&real)), (env.fee_vault(), env.fee_vault()));
    // Nobody but the keeper records a migration someone else ran: not the
    // twin, nor even DBC's own position.
    for nft in [twin, real] {
        let ix = env.graduate_ix(&ak.pubkey(), &c, &nft, None);
        expect_err(env.send_v0(&[ix], &ak, &[]), LaunchError::OnlyKeeper);
    }
    // A twin its maker kept is not the vault's, even named by the keeper.
    let kept = env.locked_position(&ak, &c, ours, false).unwrap();
    let k = env.keeper.insecure_clone();
    let ix = env.graduate_ix(&k.pubkey(), &c, &kept, None);
    expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::NotLocked);
    // The keeper records DBC's.
    let ix = env.graduate_ix(&k.pubkey(), &c, &real, None);
    env.send_v0(&[ix], &k, &[]).unwrap();
    let l = env.launch_of(&c);
    assert_eq!((l.position, l.locked_liquidity), (pda(&[b"position", real.as_ref()], &DAMM_ID), ours));
    // A coin's pool is never swept to the platform, whichever position.
    for nft in [real, twin] {
        let ix = env.sweep_position_ix(&k.pubkey(), &c.damm_pool(), &c.base_mint, &c.ptoken, &nft);
        expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::IsCoin);
    }
}

/// Two coins at the same price and gradUsd lock the same liquidity: one's
/// position is never recorded as the other's.
#[test]
fn graduate_takes_only_a_position_in_the_coins_own_pool() {
    let mut env = Env::new();
    let (_, a) = launched(&mut env, 1, &LaunchArgs::default());
    let (_, b) = launched(&mut env, 2, &LaunchArgs::default());
    complete(&mut env, &a);
    complete(&mut env, &b);
    let (_, b_nft) = env.migrate(&b).unwrap();
    let k = env.keeper.insecure_clone();
    let (ix, m1, m2) = env.migrate_ix(&k.pubkey(), &a);
    env.send(&[ix], &k, &[&m1, &m2]).unwrap();
    let a_locked = env.position_state(&pda(&[b"position", m1.pubkey().as_ref()], &DAMM_ID)).permanent_locked_liquidity;
    assert_eq!(env.launch_of(&b).locked_liquidity, a_locked);
    // B's position: the vault's, locked, the very liquidity A expects.
    let ix = env.graduate_ix(&k.pubkey(), &a, &b_nft, None);
    expect_err(env.send_v0(&[ix], &k, &[]), LaunchError::WrongAccount);
    let ix = env.graduate_ix(&k.pubkey(), &a, &m1.pubkey(), None);
    env.send_v0(&[ix], &k, &[]).unwrap();
    assert_eq!(env.launch_of(&a).position, pda(&[b"position", m1.pubkey().as_ref()], &DAMM_ID));
}

/// Whoever calls DBC's migration cannot pick the pool: DBC refuses every
/// other DAMM v2 config for our fee option, and `graduate` accepts only the
/// pool under `Hv8L…`.
#[test]
fn migration_cannot_be_redirected_to_another_config() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    complete(&mut env, &c);
    let attacker = env.user(0);
    let ak = attacker.kp.insecure_clone();
    // The two other kinds of config only DBC may use: a static one at 2%
    // and the customizable one. DBC refuses both: our fee option takes a
    // flat 1% only (InvalidMigrationFeeOption).
    for other in [DAMM_CONFIG_2PCT, DAMM_CONFIG_CUSTOM] {
        let (ix, m1, m2) = env.migrate_ix_into(&ak.pubkey(), &c, &other);
        expect_custom(env.send(&[ix], &ak, &[&m1, &m2]), 1, 6010);
        assert!(!env.exists(&damm_pool_address(&other, &c.base_mint, &c.ptoken)));
    }
    // `graduate` given another config or another pool address.
    let k = env.keeper.insecure_clone();
    let (m1, m2) = (Keypair::new(), Keypair::new());
    let mut ix = env.graduate_ix(&k.pubkey(), &c, &m1.pubkey(), Some(&m2.pubkey()));
    let other_pool = damm_pool_address(&DAMM_CONFIG_2PCT, &c.base_mint, &c.ptoken);
    for m in ix.accounts.iter_mut().filter(|m| m.pubkey == DAMM_CONFIG) {
        m.pubkey = DAMM_CONFIG_2PCT;
    }
    expect_err(env.send_v0(&[ix.clone()], &k, &[&m1, &m2]), LaunchError::WrongAccount);
    for m in ix.accounts.iter_mut() {
        if m.pubkey == DAMM_CONFIG_2PCT {
            m.pubkey = DAMM_CONFIG;
        } else if m.pubkey == c.damm_pool() {
            m.pubkey = other_pool;
        }
    }
    expect_err(env.send_v0(&[ix], &k, &[&m1, &m2]), LaunchError::WrongAccount);
    // Migrating through Hv8L is all anyone can do, and the position is the vault's.
    env.migrate(&c).unwrap();
    assert!(env.launch_of(&c).graduated);
}

/// The band's edges and gradUsd's bounds, through the real DBC: the config
/// it computes is ours, the curve completes, `graduate` migrates with the
/// vault's position holding exactly the expected liquidity, and the pool's
/// fees arrive in pToken only, whichever way a trade goes.
#[test]
fn launch_to_fees_at_the_band_edges_and_grad_bounds() {
    for (grad, price) in [
        (1_000 * USDC, 1_000u64),
        (1_000 * USDC, 990_000),
        (1_000_000 * USDC, 1_000),
        (1_000_000 * USDC, 990_000),
        (6_000 * USDC, 1_000),
        (6_000 * USDC, 990_000),
    ] {
        let mut env = Env::new();
        let o = env.owner.insecure_clone();
        let platform = env.platform.pubkey();
        env.owner_ix(&o, pl::instruction::SetConfig { platform, grad_usd: grad }).unwrap();
        let creator = env.user(10 * USDC);
        let (c, _) = env.launch(&creator, pid(1), price, &LaunchArgs::default()).unwrap();
        let curve = pl::curve::curve(grad, price).unwrap();
        let p = env.pool_config(&c);
        assert_eq!(
            (p.migration_quote_threshold, p.swap_base_amount, p.migration_base_threshold, p.migration_sqrt_price),
            (curve.threshold, curve.swap_base_amount, curve.migration_base_threshold, curve.migration_sqrt_price),
            "{grad} {price}"
        );

        // One buy past the threshold: PartialFill stops at the migration price.
        let whale = env.user(grad * 11 / 10);
        env.warp(120);
        env.buy(&whale, &c, grad * 21 / 20, price).unwrap();
        let pool = env.curve_pool(&c.pool).pool_state;
        assert_eq!(pool.migration_progress, 2, "{grad} {price}");
        assert!(pool.quote_reserve >= curve.threshold && pool.quote_reserve <= curve.threshold + 1);

        let (sent, _) = env.migrate(&c).unwrap();
        eprintln!("gradUsd {} at {price}: graduate {} CU, {} bytes", grad / USDC, sent.cu, sent.size);
        let l = env.launch_of(&c);
        let pos = env.position_state(&l.position);
        let dp = env.damm_pool_state(&c);
        assert_eq!(l.locked_liquidity, pos.permanent_locked_liquidity);
        assert_eq!(pos.permanent_locked_liquidity, dp.liquidity, "no dust position at {grad} {price}");
        assert_eq!(env.supply(&c.base_mint), curve.post_migration_supply);

        // A buy and a sell on the pool; the vault gets pToken only.
        let wp = get_associated_token_address(&whale.key(), &c.ptoken);
        let wk = get_associated_token_address(&whale.key(), &c.base_mint);
        let w = whale.kp.insecure_clone();
        let ix = env.pool_swap_ix(&whale.key(), &c, true, env.balance(&wp) / 2);
        env.send_v0(&[ix], &w, &[]).unwrap();
        let ix = env.pool_swap_ix(&whale.key(), &c, false, env.balance(&wk) / 100);
        env.send_v0(&[ix], &w, &[]).unwrap();
        env.collect(&c).unwrap();
        assert!(env.launch_of(&c).pool_fees > 0, "{grad} {price}");
        assert_eq!(env.balance(&get_associated_token_address(&env.fee_vault(), &c.base_mint)), 0);
    }
}

/* -------------------------------------------------------------------- fees */

#[test]
fn fees_are_claimed_before_and_after_migration_and_split_per_coin() {
    let mut env = Env::new();
    let a = LaunchArgs { holders_bps: 2_500, ..Default::default() };
    let (creator, c) = launched(&mut env, 1, &a);
    let platform = env.platform.pubkey();
    env.warp(120);
    let u = env.user(1_000 * USDC);
    env.buy(&u, &c, 500 * USDC, P60).unwrap();

    // Curve fees: 1.4% less Meteora's 20%; split 50/50, a quarter of the
    // creator side held for holders.
    let partner = env.curve_pool(&c.pool).pool_state.partner_quote_fee;
    let sent = env.collect(&c).unwrap();
    report("collect (curve)", &sent);
    let l = env.launch_of(&c);
    assert_eq!(l.curve_fees, partner);
    let creator_side = partner / 2;
    let holders = creator_side / 4;
    assert_eq!(l.holders_owed, holders);
    assert_eq!(env.owed(&c.ptoken, &creator.key()), creator_side - holders);
    assert_eq!(env.platform_owed(&c.ptoken, &platform), partner - creator_side);
    let vault = get_associated_token_address(&env.fee_vault(), &c.ptoken);
    assert_eq!(env.balance(&vault), partner);
    // A second collect finds nothing new.
    env.collect(&c).unwrap();
    assert_eq!(env.launch_of(&c).curve_fees, partner);

    // Withdrawals: the payee and the platform each take theirs; the keeper
    // moves the holders' part to the payout wallet only.
    let cp = env.ata(&creator.key(), &c.ptoken);
    let ck = creator.kp.insecure_clone();
    env.withdraw(&ck, &c.ptoken, &cp).unwrap();
    assert_eq!(env.balance(&cp), creator_side - holders);
    expect_err(env.withdraw(&ck, &c.ptoken, &cp), LaunchError::NothingToClaim);
    let pp = env.ata(&platform, &c.ptoken);
    let pk = env.platform.insecure_clone();
    env.withdraw_as(&pk, &c.ptoken, &pp, true).unwrap();
    assert_eq!(env.balance(&pp), partner - creator_side);
    let payout = env.ata(&env.holder_payout.pubkey(), &c.ptoken);
    let keeper = env.keeper.insecure_clone();
    let stranger = env.user(0);
    let sp = env.ata(&stranger.key(), &c.ptoken);
    expect_custom(env.withdraw_holders(&keeper, &c, holders, &sp), 1, 2015); // ConstraintTokenOwner
    expect_err(env.withdraw_holders(&ck, &c, holders, &payout), LaunchError::OnlyKeeper);
    expect_err(env.withdraw_holders(&keeper, &c, holders + 1, &payout), LaunchError::NothingToClaim);
    env.withdraw_holders(&keeper, &c, holders, &payout).unwrap();
    assert_eq!(env.balance(&payout), holders);
    assert_eq!(env.balance(&vault), 0);

    // Graduate, trade on the pool, collect again: pool fees split the same way.
    complete(&mut env, &c);
    env.migrate(&c).unwrap();
    let mid = env.launch_of(&c);
    let curve_before = mid.curve_fees;
    let whale = env.user(2_000 * USDC);
    env.ata(&whale.key(), &c.ptoken);
    env.ata(&whale.key(), &c.base_mint);
    let q = env.quote(c.pid, BUY, P60, 1_000 * USDC);
    let shares = env.minted(P60, 1_000 * USDC);
    let ixs = vec![env.ed25519_ix(&q), env.mint_ix(&whale, &c, 1_000 * USDC, &q), env.pool_swap_ix(&whale.key(), &c, true, shares)];
    let w = whale.kp.insecure_clone();
    let sent = env.send_v0(&ixs, &w, &[]).unwrap();
    report("buy after graduation: ed25519 + exchange mint + DAMM v2 swap2", &sent);

    let sent = env.collect(&c).unwrap();
    report("collect (curve + pool)", &sent);
    let l = env.launch_of(&c);
    let pool_fees = l.pool_fees;
    let curve_new = l.curve_fees - curve_before;
    eprintln!("pool fees on {shares} pToken in: {pool_fees} ({} bps of input)", pool_fees as u128 * 10_000 / shares as u128);
    // 1% base (plus the dynamic fee) less Meteora's 20%: about 0.8% to us.
    assert!(pool_fees * 10_000 >= shares * 79 && pool_fees * 10_000 <= shares * 100, "{pool_fees}");
    let total = curve_new + pool_fees;
    let creator_side = curve_new / 2 + pool_fees / 2;
    assert!(env.owed(&c.ptoken, &creator.key()) + env.platform_owed(&c.ptoken, &platform) + l.holders_owed == total);
    assert!(env.platform_owed(&c.ptoken, &platform) >= total - creator_side);
}

#[test]
fn payee_moves_in_two_steps_and_owed_fees_stay_with_the_old_one() {
    let mut env = Env::new();
    let (creator, c) = launched(&mut env, 1, &LaunchArgs::default());
    env.warp(120);
    let u = env.user(1_000 * USDC);
    env.buy(&u, &c, 100 * USDC, P60).unwrap();
    env.collect(&c).unwrap();
    let owed_old = env.owed(&c.ptoken, &creator.key());
    assert!(owed_old > 0);

    let new = Keypair::new();
    env.svm.airdrop(&new.pubkey(), 1_000_000_000).unwrap();
    let ck = creator.kp.insecure_clone();
    expect_err(env.payee_ix(&new, &c, pl::instruction::ProposePayee { payee: new.pubkey() }), LaunchError::OnlyPayee);
    env.payee_ix(&ck, &c, pl::instruction::ProposePayee { payee: new.pubkey() }).unwrap();
    expect_err(env.payee_ix(&ck, &c, pl::instruction::AcceptPayee {}), LaunchError::NotPendingPayee);
    env.payee_ix(&new, &c, pl::instruction::AcceptPayee {}).unwrap();
    assert_eq!(env.launch_of(&c).payee, new.pubkey());

    env.buy(&u, &c, 100 * USDC, P60).unwrap();
    env.collect(&c).unwrap();
    assert_eq!(env.owed(&c.ptoken, &creator.key()), owed_old);
    assert!(env.owed(&c.ptoken, &new.pubkey()) > 0);
}

/// The owner may later make the platform a wallet that is already some
/// coin's payee (or a payee may become the platform's wallet): that coin's
/// fees still collect, each side to its own account, and both withdraw.
#[test]
fn collect_works_when_the_platform_is_the_payee() {
    let mut env = Env::new();
    let (creator, c) = launched(&mut env, 1, &LaunchArgs::default());
    env.warp(120);
    let u = env.user(1_000 * USDC);
    env.buy(&u, &c, 100 * USDC, P60).unwrap();
    let o = env.owner.insecure_clone();
    env.owner_ix(&o, pl::instruction::SetConfig { platform: creator.key(), grad_usd: 6_000 * USDC }).unwrap();
    env.platform = creator.kp.insecure_clone();
    let partner = env.curve_pool(&c.pool).pool_state.partner_quote_fee;
    env.collect(&c).unwrap();
    let (as_payee, as_platform) = (env.owed(&c.ptoken, &creator.key()), env.platform_owed(&c.ptoken, &creator.key()));
    assert_eq!((as_payee, as_platform), (partner / 2, partner - partner / 2));
    let ck = creator.kp.insecure_clone();
    let to = env.ata(&creator.key(), &c.ptoken);
    env.withdraw(&ck, &c.ptoken, &to).unwrap();
    env.withdraw_as(&ck, &c.ptoken, &to, true).unwrap();
    assert_eq!(env.balance(&to), partner);
    // The platform's wallet may launch a coin of its own.
    let (c2, _) = env.launch(&creator, pid(2), P60, &LaunchArgs::default()).unwrap();
    assert_eq!(env.launch_of(&c2).payee, creator.key());
}

/// What `launch` reads back from DBC covers every field DBC could change.
#[test]
fn check_config_reads_back_every_field() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    let acc = env.svm.get_account(&c.config).unwrap();
    let curve = pl::curve::curve(6_000 * USDC, P60).unwrap();
    let asked = config_parameters(&curve, &Fees::DEFAULT);
    let check = |data: &mut [u8]| {
        let mut lamports = acc.lamports;
        let ai = anchor_lang::prelude::AccountInfo::new(&c.config, false, false, &mut lamports, data, &acc.owner, false);
        pl::meteora::check_config(&ai, &c.ptoken, &env.fee_vault(), &curve, &asked)
    };
    let mut data = acc.data.clone();
    check(&mut data).unwrap();
    use dbc::accounts::PoolConfig as P;
    use std::mem::offset_of;
    let fields = [
        8 + offset_of!(P, pool_fees.base_fee.cliff_fee_numerator),
        8 + offset_of!(P, pool_fees.base_fee.first_factor),
        8 + offset_of!(P, pool_fees.base_fee.third_factor),
        8 + offset_of!(P, pool_fees.base_fee.base_fee_mode),
        8 + offset_of!(P, collect_fee_mode),
        8 + offset_of!(P, migration_option),
        8 + offset_of!(P, migration_fee_option),
        8 + offset_of!(P, activation_type),
        8 + offset_of!(P, creator_trading_fee_percentage),
        8 + offset_of!(P, token_update_authority),
        8 + offset_of!(P, enable_first_swap_with_min_fee),
        8 + offset_of!(P, pool_fees.dynamic_fee.initialized),
        8 + offset_of!(P, token_type),
        8 + offset_of!(P, token_decimal),
        8 + offset_of!(P, partner_liquidity_percentage),
        8 + offset_of!(P, partner_permanent_locked_liquidity_percentage),
        8 + offset_of!(P, creator_liquidity_percentage),
        8 + offset_of!(P, creator_permanent_locked_liquidity_percentage),
        8 + offset_of!(P, partner_liquidity_vesting_info.vesting_percentage),
        8 + offset_of!(P, creator_liquidity_vesting_info.frequency),
        8 + offset_of!(P, locked_vesting_config.amount_per_period),
        8 + offset_of!(P, migration_fee_percentage),
        8 + offset_of!(P, creator_migration_fee_percentage),
        8 + offset_of!(P, fixed_token_supply_flag),
        8 + offset_of!(P, migrated_collect_fee_mode),
        8 + offset_of!(P, migrated_dynamic_fee),
        8 + offset_of!(P, migrated_pool_fee_bps),
        8 + offset_of!(P, migrated_pool_base_fee_mode),
        8 + offset_of!(P, migrated_compounding_fee_bps),
        8 + offset_of!(P, migrated_pool_base_fee_bytes),
        8 + offset_of!(P, pool_creation_fee),
        8 + offset_of!(P, pre_migration_token_supply),
        8 + offset_of!(P, post_migration_token_supply),
        8 + offset_of!(P, sqrt_start_price),
        8 + offset_of!(P, curve),
        8 + offset_of!(P, curve) + 16,
        8 + offset_of!(P, curve) + 32,
        8 + offset_of!(P, swap_base_amount),
        8 + offset_of!(P, migration_quote_threshold),
        8 + offset_of!(P, migration_base_threshold),
        8 + offset_of!(P, migration_sqrt_price),
        8 + offset_of!(P, quote_mint),
        8 + offset_of!(P, fee_claimer),
        8 + offset_of!(P, leftover_receiver),
    ];
    for at in fields {
        let mut d = acc.data.clone();
        d[at] ^= 1;
        assert!(check(&mut d).is_err(), "field at {at} not read back");
    }
}

/// A creator who launched and kept the mint cannot open the pool later,
/// on a curve sized at a stale price.
#[test]
fn a_pool_opened_after_the_quote_expired_is_not_the_coin() {
    let mut env = Env::new();
    let creator = env.user(100_000 * USDC);
    let mint = Keypair::new();
    let c = env.coin(pid(1), mint.pubkey());
    let q = env.quote(pid(1), BUY, P60, 1);
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    ixs.truncate(2);
    let kp = creator.kp.insecure_clone();
    env.send_v0(&ixs, &kp, &[&mint]).unwrap();
    assert_eq!(env.launch_of(&c).pool_deadline, q.valid_until);
    // The record cannot be closed while the bundle may still land.
    let k = env.keeper.insecure_clone();
    expect_err(env.send(&[env.expire_ix(&c, &creator.key())], &k, &[]), LaunchError::NotExpired);

    // Days later the market has moved; the creator opens the pool anyway,
    // on the curve sized at the old price, and it trades and completes.
    env.warp(3 * 86_400);
    let init = env.pool_init_ix(&creator.key(), &creator.key(), &c, &LaunchArgs::default());
    env.send_v0(&[init], &kp, &[&mint]).unwrap();
    env.warp(120);
    env.buy(&creator, &c, 100 * USDC, P60).unwrap();
    // It is not the coin: no fee booking, no graduation.
    expect_err(env.collect(&c), LaunchError::PoolTooLate);
    let whale = complete(&mut env, &c);
    let (m1, m2) = (Keypair::new(), Keypair::new());
    let ix = env.graduate_ix(&k.pubkey(), &c, &m1.pubkey(), Some(&m2.pubkey()));
    expect_err(env.send_v0(&[ix], &k, &[&m1, &m2]), LaunchError::PoolTooLate);
    // Its fees can only be swept (once the record is gone), and anyone may
    // close the record, the rent going back to the creator.
    let sweep = env.sweep_ix(&k.pubkey(), &c.config, &c.pool, &c.base_mint, &c.ptoken);
    expect_err(env.send_v0(std::slice::from_ref(&sweep), &k, &[]), LaunchError::IsCoin);
    let rent = env.svm.get_account(&c.launch).unwrap().lamports;
    let before = env.svm.get_account(&creator.key()).unwrap().lamports;
    let stranger = env.user(0);
    let sk = stranger.kp.insecure_clone();
    env.send(&[env.expire_ix(&c, &creator.key())], &sk, &[]).unwrap();
    assert!(!env.exists(&c.launch));
    assert_eq!(env.svm.get_account(&creator.key()).unwrap().lamports, before + rent);
    let partner = env.curve_pool(&c.pool).pool_state.partner_quote_fee;
    env.send_v0(&[sweep], &k, &[]).unwrap();
    let platform = env.platform.pubkey();
    assert_eq!(env.platform_owed(&c.ptoken, &platform), partner);

    // Anyone may still run DBC's migration on it: the coin's config hands
    // our vault the pool's LP, locked for good. Its fees are the platform's.
    let (ix, m1, m2) = env.migrate_ix(&stranger.key(), &c);
    env.send(&[ix], &sk, &[&m1, &m2]).unwrap();
    assert_eq!(env.nft_owner(&m1.pubkey()), env.fee_vault());
    let w = whale.kp.insecure_clone();
    let wp = get_associated_token_address(&whale.key(), &c.ptoken);
    let ix = env.pool_swap_ix(&whale.key(), &c, true, env.balance(&wp));
    env.send_v0(&[ix], &w, &[]).unwrap();
    let position = pda(&[b"position", m1.pubkey().as_ref()], &DAMM_ID);
    let sweep = env.sweep_position_ix(&k.pubkey(), &c.damm_pool(), &c.base_mint, &c.ptoken, &m1.pubkey());
    let mut not_keeper = sweep.clone();
    not_keeper.accounts[0].pubkey = stranger.key();
    expect_err(env.send_v0(&[not_keeper], &sk, &[]), LaunchError::OnlyKeeper);
    let vault = get_associated_token_address(&env.fee_vault(), &c.ptoken);
    let before = env.balance(&vault);
    let sent = env.send_v0(&[sweep], &k, &[]).unwrap();
    report("sweep_position_fees (late pool)", &sent);
    let fees = env.balance(&vault) - before;
    assert!(fees > 0);
    assert_eq!(env.platform_owed(&c.ptoken, &platform), partner + fees);
    assert_eq!(env.platform_owed(&c.base_mint, &platform), 0);
    assert_eq!(env.position_state(&position).metrics.total_claimed_b_fee, fees);
    // The platform takes it all.
    let pk = env.platform.insecure_clone();
    let to = env.ata(&platform, &c.ptoken);
    env.withdraw_as(&pk, &c.ptoken, &to, true).unwrap();
    assert_eq!(env.balance(&to), partner + fees);
    // The coin can never be launched again on its config.
    let q = env.quote(pid(1), BUY, P60, 1);
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    ixs.truncate(2);
    assert!(env.send_v0(&ixs, &kp, &[&mint]).is_err());
}

/// A launch whose second transaction never landed: after the quote's
/// expiry anyone may close the record; a pool opened in time keeps it.
#[test]
fn a_launch_without_its_pool_expires() {
    let mut env = Env::new();
    let creator = env.user(1_000 * USDC);
    let mint = Keypair::new();
    let c = env.coin(pid(1), mint.pubkey());
    let q = env.quote(pid(1), BUY, P60, 1);
    let mut ixs = env.launch_txs(&creator, &c, &q, &LaunchArgs::default()).remove(0);
    ixs.truncate(2);
    let kp = creator.kp.insecure_clone();
    env.send_v0(&ixs, &kp, &[&mint]).unwrap();
    let k = env.keeper.insecure_clone();
    // The rent goes to the creator only.
    env.warp(16);
    let mut wrong = env.expire_ix(&c, &creator.key());
    wrong.accounts[1].pubkey = k.pubkey();
    expect_err(env.send(&[wrong], &k, &[]), LaunchError::WrongAccount);
    env.send(&[env.expire_ix(&c, &creator.key())], &k, &[]).unwrap();
    assert!(!env.exists(&c.launch));

    // A coin whose pool opened with its launch never expires...
    let (_, ok) = launched(&mut env, 2, &LaunchArgs::default());
    env.warp(86_400);
    let creator = env.launch_of(&ok).creator;
    expect_err(env.send(&[env.expire_ix(&ok, &creator)], &k, &[]), LaunchError::NotExpired);
    // ...whatever is passed as its pool: no account, or another coin's
    // real pool opened after this one's deadline.
    let (_, later) = launched(&mut env, 3, &LaunchArgs::default());
    for pool in [Keypair::new().pubkey(), later.pool] {
        let mut ix = env.expire_ix(&ok, &creator);
        ix.accounts[2].pubkey = pool;
        expect_err(env.send(&[ix], &k, &[]), LaunchError::WrongAccount);
    }
    assert!(env.exists(&ok.launch));
}

#[test]
fn leftover_coins_are_burned() {
    let mut env = Env::new();
    let (_, c) = launched(&mut env, 1, &LaunchArgs::default());
    env.collect(&c).unwrap(); // creates the vault's coin account
    let vault_coin = get_associated_token_address(&env.fee_vault(), &c.base_mint);
    let k = env.keeper.insecure_clone();
    let burn = Instruction {
        program_id: pl::ID,
        accounts: pl::accounts::BurnLeftover {
            factory: env.factory(),
            launch: c.launch,
            fee_vault: env.fee_vault(),
            base_mint: c.base_mint,
            vault_coin,
            token_program: anchor_spl::token::ID,
            event_authority: env.events(),
            program: pl::ID,
        }
        .to_account_metas(None),
        data: pl::instruction::BurnLeftover {}.data(),
    };
    // Before graduation there is no leftover: whatever the vault holds in
    // the coin is not burned.
    env.set_token(vault_coin, c.base_mint, env.fee_vault(), 5);
    expect_err(env.send(std::slice::from_ref(&burn), &k, &[]), LaunchError::NotMigrated);
    env.set_token(vault_coin, c.base_mint, env.fee_vault(), 0);
    complete(&mut env, &c);
    env.migrate(&c).unwrap();
    let ix = Instruction {
        program_id: DBC_ID,
        accounts: dbc::client::accounts::WithdrawLeftover {
            pool_authority: DBC_POOL_AUTHORITY,
            config: c.config,
            virtual_pool: c.pool,
            token_base_account: vault_coin,
            base_vault: c.base_vault,
            base_mint: c.base_mint,
            leftover_receiver: env.fee_vault(),
            token_base_program: anchor_spl::token::ID,
            event_authority: DBC_EVENT_AUTHORITY,
            program: DBC_ID,
        }
        .to_account_metas(None),
        data: dbc::client::args::WithdrawLeftover {}.data(),
    };
    let r = env.send(&[ix], &k, &[]).unwrap();
    let left = env.balance(&vault_coin);
    eprintln!("withdraw_leftover: {} CU, leftover {left}", r.cu);
    // DBC's rounding leaves 3 atoms above the burn cap at 60c.
    assert_eq!(left, 3);
    let supply = env.supply(&c.base_mint);
    env.send(std::slice::from_ref(&burn), &k, &[]).unwrap();
    assert_eq!(env.supply(&c.base_mint), supply - left);
    assert_eq!(env.balance(&vault_coin), 0);
    // Nothing left: a second burn is refused.
    expect_err(env.send(&[burn], &k, &[]), LaunchError::NothingToClaim);
}

/* ------------------------------------------------------------------- admin */

#[test]
fn factory_settings_are_bounded_and_owner_only() {
    let mut env = Env::new();
    let o = env.owner.insecure_clone();
    let stranger = Keypair::new();
    env.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    let platform = env.platform.pubkey();
    expect_err(env.owner_ix(&stranger, pl::instruction::SetConfig { platform, grad_usd: 6_000 * USDC }), LaunchError::OnlyOwner);
    expect_err(env.owner_ix(&o, pl::instruction::SetConfig { platform, grad_usd: 999 * USDC }), LaunchError::BadParams);
    expect_err(env.owner_ix(&o, pl::instruction::SetConfig { platform, grad_usd: 1_000_001 * USDC }), LaunchError::BadParams);
    expect_err(
        env.owner_ix(&o, pl::instruction::SetConfig { platform: Pubkey::default(), grad_usd: 6_000 * USDC }),
        LaunchError::BadParams,
    );
    env.owner_ix(&o, pl::instruction::SetConfig { platform, grad_usd: 12_000 * USDC }).unwrap();
    assert_eq!(env.factory_state().grad_usd, 12_000 * USDC);
    for bad in [
        Fees { curve_fee_bps: 201, ..Fees::DEFAULT },
        Fees { curve_fee_bps: 24, ..Fees::DEFAULT },
        Fees { pool_fee_bps: 125, ..Fees::DEFAULT },
        Fees { curve_creator_share_bps: 8_001, ..Fees::DEFAULT },
        Fees { pool_creator_share_bps: 2_999, ..Fees::DEFAULT },
    ] {
        expect_err(env.owner_ix(&o, pl::instruction::SetFees { fees: bad }), LaunchError::BadFees);
    }
    let fees = Fees { curve_fee_bps: 175, curve_creator_share_bps: 6_000, ..Fees::DEFAULT };
    env.owner_ix(&o, pl::instruction::SetFees { fees }).unwrap();

    // A coin launched now is sized by the new gradUsd and keeps these fees.
    let creator = env.user(1_000 * USDC);
    let (c, _) = env.launch(&creator, pid(3), P60, &LaunchArgs::default()).unwrap();
    let l = env.launch_of(&c);
    assert_eq!((l.fees, l.threshold), (fees, 20_000_000_000));
    assert_eq!(env.pool_config(&c).pool_fees.base_fee.cliff_fee_numerator, 17_500_000 + SNIPER_EXTRA_NUMERATOR);

    // Two-step ownership.
    env.owner_ix(&o, pl::instruction::TransferOwnership { new_owner: stranger.pubkey() }).unwrap();
    let accept = |env: &mut Env, who: &Keypair| {
        let accounts = pl::accounts::AcceptOwnership {
            pending_owner: who.pubkey(),
            factory: env.factory(),
            event_authority: env.events(),
            program: pl::ID,
        };
        let ix =
            Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: pl::instruction::AcceptOwnership {}.data() };
        env.send(&[ix], who, &[])
    };
    expect_err(accept(&mut env, &o), LaunchError::NotPendingOwner);
    accept(&mut env, &stranger).unwrap();
    assert_eq!(env.factory_state().owner, stranger.pubkey());
}

