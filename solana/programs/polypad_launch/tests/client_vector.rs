//! The TypeScript client's cross-language check (lib/solana/): every PDA it
//! derives, the instruction data it encodes and the account lists it sends,
//! worked out here from the programs' own seeds and Anchor client types and
//! written to `tests/fixtures/client_v1.json`. lib/solana/client.test.ts
//! derives the same from the fixture's inputs and must match it exactly, so
//! a seed, argument or account-order change on either side fails a test.
//!
//! After a deliberate change, rewrite the fixture with
//! `WRITE_CLIENT_FIXTURE=1 cargo test -p polypad_launch --test client_vector`
//! and run `npm test`. Needs no Meteora binaries: nothing is executed.
//!
//! The quote comes from the exchange's `quote_v1.json` (a devnet quote), so
//! this only runs without the `mainnet` feature.
#![cfg(not(feature = "mainnet"))]

use std::str::FromStr;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::AccountMeta;
use anchor_lang::{system_program, InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::spl_token;
use polypad_exchange as px;
use polypad_launch as pl;
use pl::cp_amm;
use pl::dynamic_bonding_curve as dbc;
use pl::meteora::*;
use pl::state::*;
use px::quote::{Quote, BUY, SELL};
use serde_json::{json, Value};

const QUOTE_FIXTURE: &str = include_str!("../../polypad_exchange/tests/fixtures/quote_v1.json");
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/client_v1.json");
const INSTRUCTIONS: Pubkey = anchor_lang::pubkey!("Sysvar1nstructions1111111111111111111111111");
const DEVNET_USDC: Pubkey = anchor_lang::pubkey!("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

fn key(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn quote_field(name: &str) -> String {
    let v: Value = serde_json::from_str(QUOTE_FIXTURE).unwrap();
    v[name].as_str().unwrap_or_else(|| panic!("quote fixture has no {name}")).to_string()
}

fn quote(side: u8) -> Quote {
    let h = quote_field("position_id_hex");
    let id: Vec<u8> = (0..64).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect();
    Quote {
        position_id: id.try_into().unwrap(),
        side,
        price: quote_field("price").parse().unwrap(),
        max_amount: quote_field("max_amount").parse().unwrap(),
        valid_until: quote_field("valid_until").parse().unwrap(),
    }
}

fn metas(m: Vec<AccountMeta>) -> Value {
    Value::Array(m.into_iter().map(|a| json!([a.pubkey.to_string(), a.is_signer, a.is_writable])).collect())
}

fn vector() -> Value {
    let q = quote(BUY);
    let id = q.position_id;
    let fill = pda(&[px::state::FILL_SEED, &q.fill_seed()], &px::ID);
    let n: u64 = 7;

    let (creator, base_mint, platform, nft_mint, user_usdc) = (key(0x22), key(0x11), key(0x33), key(0x44), key(0x55));
    let ex_config = pda(&[px::state::CONFIG_SEED], &px::ID);
    let float = pda(&[px::state::VAULT_SEED], &px::ID);
    let market = pda(&[px::state::MARKET_SEED, &id], &px::ID);
    let ptoken = pda(&[px::state::PTOKEN_SEED, &id], &px::ID);
    let held = pda(&[px::state::HELD_SEED, &id], &px::ID);
    let ex_events = pda(&[b"__event_authority"], &px::ID);
    let factory = pda(&[FACTORY_SEED], &pl::ID);
    let fee_vault = pda(&[FEE_VAULT_SEED], &pl::ID);
    let events = pda(&[b"__event_authority"], &pl::ID);
    let dbc_config = pda(&[DBC_CONFIG_SEED, base_mint.as_ref()], &pl::ID);
    let launch = pda(&[LAUNCH_SEED, base_mint.as_ref()], &pl::ID);
    let pool = pool_address(&dbc_config, &base_mint, &ptoken);
    let base_vault = pda(&[b"token_vault", base_mint.as_ref(), pool.as_ref()], &DBC_ID);
    let quote_vault = pda(&[b"token_vault", ptoken.as_ref(), pool.as_ref()], &DBC_ID);
    let damm_pool = damm_pool_address(&DAMM_CONFIG, &base_mint, &ptoken);
    let damm_a = pda(&[b"token_vault", base_mint.as_ref(), damm_pool.as_ref()], &DAMM_ID);
    let damm_b = pda(&[b"token_vault", ptoken.as_ref(), damm_pool.as_ref()], &DAMM_ID);
    let creator_ptoken = get_associated_token_address(&creator, &ptoken);
    let creator_coin = get_associated_token_address(&creator, &base_mint);
    let claim = pda(&[px::state::CLAIM_SEED, &n.to_le_bytes()], &px::ID);
    let delayed = pda(&[px::state::DELAYED_SEED, &n.to_le_bytes()], &px::ID);

    let mint_ix = px::accounts::MintQuoted {
        buyer: creator,
        config: ex_config,
        market,
        ptoken_mint: ptoken,
        buyer_usdc: user_usdc,
        to: creator_ptoken,
        vault: float,
        usdc_mint: DEVNET_USDC,
        fill,
        instructions: INSTRUCTIONS,
        token_program: spl_token::ID,
        system_program: system_program::ID,
        event_authority: ex_events,
        program: px::ID,
    };
    let sq = quote(SELL);
    let sell_fill = pda(&[px::state::FILL_SEED, &sq.fill_seed()], &px::ID);
    let redeem_ix = px::accounts::RedeemQuoted {
        seller: creator,
        config: ex_config,
        market,
        ptoken_mint: ptoken,
        seller_ptoken: creator_ptoken,
        held,
        to: user_usdc,
        vault: float,
        usdc_mint: DEVNET_USDC,
        fill: sell_fill,
        instructions: INSTRUCTIONS,
        new_claim: Some(claim),
        new_ticket: Some(delayed),
        token_program: spl_token::ID,
        system_program: system_program::ID,
        event_authority: ex_events,
        program: px::ID,
    };
    let launch_ix = pl::accounts::LaunchCoin {
        creator,
        factory,
        fee_vault,
        exchange_config: ex_config,
        market,
        ptoken_mint: ptoken,
        held,
        dbc_config,
        base_mint,
        launch,
        instructions: INSTRUCTIONS,
        exchange_event_authority: ex_events,
        exchange_program: px::ID,
        dbc_event_authority: DBC_EVENT_AUTHORITY,
        dbc_program: DBC_ID,
        token_program: spl_token::ID,
        system_program: system_program::ID,
        buyer_usdc: Some(user_usdc),
        creator_ptoken: Some(creator_ptoken),
        usdc_vault: Some(float),
        usdc_mint: Some(DEVNET_USDC),
        fill: Some(fill),
        associated_token_program: Some(anchor_spl::associated_token::ID),
        event_authority: events,
        program: pl::ID,
    };
    // Without a first buy the optional accounts are absent: Anchor clients
    // pass the program id in their place.
    let launch_bare = pl::accounts::LaunchCoin {
        buyer_usdc: None,
        creator_ptoken: None,
        usdc_vault: None,
        usdc_mint: None,
        fill: None,
        associated_token_program: None,
        ..launch_ix.clone()
    };
    let curve_swap = dbc::client::accounts::Swap2 {
        pool_authority: DBC_POOL_AUTHORITY,
        config: dbc_config,
        pool,
        input_token_account: creator_ptoken,
        output_token_account: creator_coin,
        base_vault,
        quote_vault,
        base_mint,
        quote_mint: ptoken,
        payer: creator,
        token_base_program: spl_token::ID,
        token_quote_program: spl_token::ID,
        referral_token_account: None,
        event_authority: DBC_EVENT_AUTHORITY,
        program: DBC_ID,
    };
    let pool_swap = cp_amm::client::accounts::Swap2 {
        pool_authority: DAMM_POOL_AUTHORITY,
        pool: damm_pool,
        input_token_account: creator_coin,
        output_token_account: creator_ptoken,
        token_a_vault: damm_a,
        token_b_vault: damm_b,
        token_a_mint: base_mint,
        token_b_mint: ptoken,
        payer: creator,
        token_a_program: spl_token::ID,
        token_b_program: spl_token::ID,
        referral_token_account: None,
        event_authority: DAMM_EVENT_AUTHORITY,
        program: DAMM_ID,
    };
    let pool_init = dbc::client::accounts::InitializeVirtualPoolWithSplToken {
        config: dbc_config,
        pool_authority: DBC_POOL_AUTHORITY,
        creator,
        base_mint,
        quote_mint: ptoken,
        pool,
        base_vault,
        quote_vault,
        mint_metadata: pda(&[b"metadata", METADATA_PROGRAM.as_ref(), base_mint.as_ref()], &METADATA_PROGRAM),
        metadata_program: METADATA_PROGRAM,
        payer: creator,
        token_quote_program: spl_token::ID,
        token_program: spl_token::ID,
        system_program: system_program::ID,
        event_authority: DBC_EVENT_AUTHORITY,
        program: DBC_ID,
    };
    let collect = pl::accounts::Collect {
        caller: creator,
        factory,
        launch,
        fee_vault,
        ptoken_mint: ptoken,
        base_mint,
        vault_ptoken: get_associated_token_address(&fee_vault, &ptoken),
        vault_coin: get_associated_token_address(&fee_vault, &base_mint),
        payee_owed: pda(&[OWED_SEED, ptoken.as_ref(), creator.as_ref()], &pl::ID),
        platform_owed: pda(&[PLATFORM_OWED_SEED, ptoken.as_ref(), platform.as_ref()], &pl::ID),
        dbc_pool_authority: DBC_POOL_AUTHORITY,
        dbc_config,
        dbc_pool: pool,
        dbc_base_vault: base_vault,
        dbc_quote_vault: quote_vault,
        dbc_event_authority: DBC_EVENT_AUTHORITY,
        dbc_program: DBC_ID,
        damm_pool_authority: Some(DAMM_POOL_AUTHORITY),
        damm_pool: Some(damm_pool),
        position: Some(pda(&[b"position", nft_mint.as_ref()], &DAMM_ID)),
        damm_token_a_vault: Some(damm_a),
        damm_token_b_vault: Some(damm_b),
        position_nft_account: Some(position_nft_account_address(&nft_mint)),
        damm_event_authority: Some(DAMM_EVENT_AUTHORITY),
        damm_program: Some(DAMM_ID),
        token_program: spl_token::ID,
        associated_token_program: anchor_spl::associated_token::ID,
        system_program: system_program::ID,
        event_authority: events,
        program: pl::ID,
    };
    // Before graduation `collect` passes none of the DAMM v2 accounts.
    let collect_curve = pl::accounts::Collect {
        damm_pool_authority: None,
        damm_pool: None,
        position: None,
        damm_token_a_vault: None,
        damm_token_b_vault: None,
        position_nft_account: None,
        damm_event_authority: None,
        damm_program: None,
        ..collect.clone()
    };
    // A sale that neither queues nor waits needs no claim or ticket slot.
    let redeem_bare = px::accounts::RedeemQuoted { new_claim: None, new_ticket: None, ..redeem_ix.clone() };
    let withdraw = pl::accounts::Withdraw {
        payee: platform,
        factory,
        fee_vault,
        owed: pda(&[OWED_SEED, ptoken.as_ref(), platform.as_ref()], &pl::ID),
        mint: ptoken,
        vault: get_associated_token_address(&fee_vault, &ptoken),
        to: user_usdc,
        token_program: spl_token::ID,
        event_authority: events,
        program: pl::ID,
    };
    // The platform withdrawing its own share: the account under its own seed.
    let withdraw_platform = pl::accounts::Withdraw { owed: pda(&[PLATFORM_OWED_SEED, ptoken.as_ref(), platform.as_ref()], &pl::ID), ..withdraw.clone() };
    let view_market = px::accounts::ViewMarket { config: ex_config, market, ptoken_mint: ptoken };
    let keeper = keeper_vector(KeeperKeys {
        creator, base_mint, platform, nft_mint, user_usdc, n, ex_config, float, market, ptoken, held, ex_events, factory, fee_vault, events, dbc_config, launch, pool,
        base_vault, quote_vault, damm_pool, damm_a, damm_b,
    });

    // [price, amount, spread bps, out], as strings: JSON numbers lose u64 precision in JavaScript.
    let mint_out = [(820_000u64, 25_000_000u64, 0u16), (820_000, 25_000_000, 150), (1_000, 1, 499), (999_000, 123_456_789_012, 37)]
        .map(|(p, a, s)| [p.to_string(), a.to_string(), s.to_string(), px::math::mint_out(p, a, s).unwrap().to_string()]);
    let redeem_out = [(820_000u64, 12_345_678u64, 0u16), (820_000, 12_345_678, 150), (1_000_000, 7_000_000, 200), (3, 1, 499)]
        .map(|(p, a, s)| [p.to_string(), a.to_string(), s.to_string(), px::math::redeem_out(p, a, s).unwrap().to_string()]);

    json!({
        "about": "Client test vector, written by solana/programs/polypad_launch/tests/client_vector.rs and checked by it and by lib/solana/client.test.ts.",
        "inputs": {
            "exchange_program": px::ID.to_string(),
            "launch_program": pl::ID.to_string(),
            "usdc_mint": DEVNET_USDC.to_string(),
            "position_id": quote_field("position_id"),
            "creator": creator.to_string(),
            "base_mint": base_mint.to_string(),
            "platform": platform.to_string(),
            "nft_mint": nft_mint.to_string(),
            "user_usdc": user_usdc.to_string(),
            "index": n.to_string(),
        },
        "pda": {
            "exchange_config": ex_config.to_string(),
            "float": float.to_string(),
            "market": market.to_string(),
            "ptoken_mint": ptoken.to_string(),
            "held": held.to_string(),
            "fill_buy": fill.to_string(),
            "fill_sell": sell_fill.to_string(),
            "claim": claim.to_string(),
            "aside": pda(&[px::state::ASIDE_SEED, &n.to_le_bytes()], &px::ID).to_string(),
            "delayed": delayed.to_string(),
            "exchange_event_authority": ex_events.to_string(),
            "factory": factory.to_string(),
            "fee_vault": fee_vault.to_string(),
            "launch_event_authority": events.to_string(),
            "dbc_config": dbc_config.to_string(),
            "launch": launch.to_string(),
            "payee_owed": pda(&[OWED_SEED, ptoken.as_ref(), creator.as_ref()], &pl::ID).to_string(),
            "dbc_pool": pool.to_string(),
            "dbc_base_vault": base_vault.to_string(),
            "dbc_quote_vault": quote_vault.to_string(),
            "damm_pool": damm_pool.to_string(),
            "damm_token_a_vault": damm_a.to_string(),
            "damm_token_b_vault": damm_b.to_string(),
            "position": pda(&[b"position", nft_mint.as_ref()], &DAMM_ID).to_string(),
            "position_nft_account": position_nft_account_address(&nft_mint).to_string(),
            "metadata": pda(&[b"metadata", METADATA_PROGRAM.as_ref(), base_mint.as_ref()], &METADATA_PROGRAM).to_string(),
            "creator_ptoken_ata": creator_ptoken.to_string(),
            "dbc_pool_authority": pda(&[b"pool_authority"], &DBC_ID).to_string(),
            "dbc_event_authority": pda(&[b"__event_authority"], &DBC_ID).to_string(),
            "damm_pool_authority": pda(&[b"pool_authority"], &DAMM_ID).to_string(),
            "damm_event_authority": pda(&[b"__event_authority"], &DAMM_ID).to_string(),
        },
        "data": {
            "mint": hex(&px::instruction::Mint { usdc_in: 25_000_000, min_out: 30_000_000, quote: q }.data()),
            "redeem": hex(&px::instruction::Redeem { amount_in: 12_345_678, min_out: 9_000_000, quote: sq }.data()),
            "mint_at_payout": hex(&px::instruction::MintAtPayout { usdc_in: 5_000_000, min_out: 4_900_000 }.data()),
            "redeem_at_payout": hex(&px::instruction::RedeemAtPayout { amount_in: 7_000_000, min_out: 6_000_000 }.data()),
            "create_market": hex(&px::instruction::CreateMarket { position_id: id }.data()),
            "view_float": hex(&px::instruction::ViewFloat {}.data()),
            "view_market": hex(&px::instruction::ViewMarket {}.data()),
            "preview_mint": hex(&px::instruction::PreviewMint { price: 820_000, usdc_in: 25_000_000 }.data()),
            "preview_redeem": hex(&px::instruction::PreviewRedeem { price: 820_000, amount_in: 12_345_678, settled: true }.data()),
            "launch": hex(&pl::instruction::Launch { quote: q, holders_bps: 2_500, first_buy: 25_000_000, first_buy_min_out: 1 }.data()),
            "collect": hex(&pl::instruction::Collect {}.data()),
            "withdraw": hex(&pl::instruction::Withdraw {}.data()),
            "dbc_swap2": hex(&dbc::client::args::Swap2 { params: dbc::types::SwapParameters2 { amount_0: 30_000_000, amount_1: 1_000, swap_mode: 1 } }.data()),
            "dbc_init_pool": hex(&dbc::client::args::InitializeVirtualPoolWithSplToken {
                params: dbc::types::InitializePoolParameters { name: "Polypad Coin".into(), symbol: "PPAD".into(), uri: "https://polypad.fun/m/abc".into() },
            }.data()),
            "damm_swap2": hex(&cp_amm::client::args::Swap2 { _params: cp_amm::types::SwapParameters2 { amount_0: 1_000_000, amount_1: 2, swap_mode: 0 } }.data()),
        },
        "accounts": {
            "mint": metas(mint_ix.to_account_metas(None)),
            "redeem": metas(redeem_ix.to_account_metas(None)),
            "redeem_no_slots": metas(redeem_bare.to_account_metas(None)),
            "launch": metas(launch_ix.to_account_metas(None)),
            "launch_no_first_buy": metas(launch_bare.to_account_metas(None)),
            "dbc_swap2_buy": metas(curve_swap.to_account_metas(None)),
            "dbc_init_pool": metas(pool_init.to_account_metas(None)),
            "damm_swap2_sell": metas(pool_swap.to_account_metas(None)),
            "collect_graduated": metas(collect.to_account_metas(None)),
            "collect_curve": metas(collect_curve.to_account_metas(None)),
            "withdraw": metas(withdraw.to_account_metas(None)),
            "withdraw_platform": metas(withdraw_platform.to_account_metas(None)),
            "view_market": metas(view_market.to_account_metas(None)),
        },
        "keeper": keeper,
        "math": {
            "mint_out": mint_out,
            "redeem_out": redeem_out,
        },
    })
}

/// Everything `vector` derived that the keeper's instructions name.
struct KeeperKeys {
    creator: Pubkey,
    base_mint: Pubkey,
    platform: Pubkey,
    nft_mint: Pubkey,
    user_usdc: Pubkey,
    n: u64,
    ex_config: Pubkey,
    float: Pubkey,
    market: Pubkey,
    ptoken: Pubkey,
    held: Pubkey,
    ex_events: Pubkey,
    factory: Pubkey,
    fee_vault: Pubkey,
    events: Pubkey,
    dbc_config: Pubkey,
    launch: Pubkey,
    pool: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    damm_pool: Pubkey,
    damm_a: Pubkey,
    damm_b: Pubkey,
}

/// The keeper's instructions (lib/solana/keeper.ts): data and accounts, with
/// remaining accounts appended in the order the programs read them.
fn keeper_vector(k: KeeperKeys) -> Value {
    let (keeper, bridge, second_nft, rent_payer) = (key(0x66), key(0x77), key(0x45), key(0x88));
    let (look_base, look_quote, look_config) = (key(0x99), key(0x9a), key(0x9b));
    let look_pool = pool_address(&look_config, &look_base, &look_quote);
    let w = |p: Pubkey| AccountMeta::new(p, false);
    let r = |p: Pubkey| AccountMeta::new_readonly(p, false);
    let with = |mut m: Vec<AccountMeta>, extra: Vec<AccountMeta>| {
        m.extend(extra);
        metas(m)
    };
    let claim = |i: u64| pda(&[px::state::CLAIM_SEED, &i.to_le_bytes()], &px::ID);
    let triple = vec![w(claim(k.n)), w(k.user_usdc), w(rent_payer)];

    let batch = px::accounts::KeeperBatch { keeper, config: k.ex_config, event_authority: k.ex_events, program: px::ID };
    let admin = px::accounts::MarketAdmin { authority: keeper, config: k.ex_config, market: k.market, event_authority: k.ex_events, program: px::ID };
    let halt = px::accounts::KeeperOrOwner { authority: keeper, config: k.ex_config, event_authority: k.ex_events, program: px::ID };
    let pay = px::accounts::PayQueue { config: k.ex_config, vault: k.float, usdc_mint: DEVNET_USDC, token_program: spl_token::ID, event_authority: k.ex_events, program: px::ID };
    let ticket = pda(&[px::state::DELAYED_SEED, &k.n.to_le_bytes()], &px::ID);
    let release = px::accounts::Release {
        caller: keeper,
        config: k.ex_config,
        market: k.market,
        ptoken_mint: k.ptoken,
        held: k.held,
        ticket,
        rent_to: rent_payer,
        to: k.user_usdc,
        vault: k.float,
        usdc_mint: DEVNET_USDC,
        new_claim: Some(claim(k.n + 1)),
        new_aside: Some(pda(&[px::state::ASIDE_SEED, &(k.n + 2).to_le_bytes()], &px::ID)),
        token_program: spl_token::ID,
        system_program: system_program::ID,
        event_authority: k.ex_events,
        program: px::ID,
    };
    let absorb = px::accounts::Absorb {
        config: k.ex_config,
        market: k.market,
        ptoken_mint: k.ptoken,
        held: k.held,
        other: Some(get_associated_token_address(&k.ex_config, &k.ptoken)),
        token_program: spl_token::ID,
        event_authority: k.ex_events,
        program: px::ID,
    };
    let send = px::accounts::SendToBridge {
        keeper,
        config: k.ex_config,
        vault: k.float,
        bridge,
        usdc_mint: DEVNET_USDC,
        token_program: spl_token::ID,
        event_authority: k.ex_events,
        program: px::ID,
    };
    let position = |m: &Pubkey| pda(&[b"position", m.as_ref()], &DAMM_ID);
    let graduate = pl::accounts::Graduate {
        payer: keeper,
        factory: k.factory,
        launch: k.launch,
        dbc_pool: k.pool,
        dbc_config: k.dbc_config,
        dbc_pool_authority: DBC_POOL_AUTHORITY,
        dbc_base_vault: k.base_vault,
        dbc_quote_vault: k.quote_vault,
        base_mint: k.base_mint,
        ptoken_mint: k.ptoken,
        damm_config: DAMM_CONFIG,
        damm_pool: k.damm_pool,
        damm_token_a_vault: k.damm_a,
        damm_token_b_vault: k.damm_b,
        position: position(&k.nft_mint),
        position_nft_account: position_nft_account_address(&k.nft_mint),
        position_nft_mint: Some(k.nft_mint),
        second_position_nft_mint: Some(second_nft),
        second_position_nft_account: Some(position_nft_account_address(&second_nft)),
        second_position: Some(position(&second_nft)),
        damm_pool_authority: DAMM_POOL_AUTHORITY,
        damm_event_authority: DAMM_EVENT_AUTHORITY,
        damm_program: DAMM_ID,
        dbc_program: DBC_ID,
        token_program: spl_token::ID,
        token_2022_program: TOKEN_2022,
        system_program: system_program::ID,
        event_authority: k.events,
        program: pl::ID,
    };
    // Recording someone else's migration: the pool's first position, no NFT mints.
    let graduate_record = pl::accounts::Graduate {
        position_nft_mint: None,
        second_position_nft_mint: None,
        second_position_nft_account: None,
        second_position: None,
        ..graduate.clone()
    };
    let expire = pl::accounts::ExpireLaunch { launch: k.launch, creator: k.creator, dbc_pool: k.pool, event_authority: k.events, program: pl::ID };
    let sweep = pl::accounts::SweepFees {
        keeper,
        factory: k.factory,
        fee_vault: k.fee_vault,
        launch: pda(&[LAUNCH_SEED, look_base.as_ref()], &pl::ID),
        quote_launch: pda(&[LAUNCH_SEED, look_quote.as_ref()], &pl::ID),
        base_mint: look_base,
        quote_mint: look_quote,
        vault_base: get_associated_token_address(&k.fee_vault, &look_base),
        vault_quote: get_associated_token_address(&k.fee_vault, &look_quote),
        base_owed: pda(&[PLATFORM_OWED_SEED, look_base.as_ref(), k.platform.as_ref()], &pl::ID),
        quote_owed: pda(&[PLATFORM_OWED_SEED, look_quote.as_ref(), k.platform.as_ref()], &pl::ID),
        dbc_pool_authority: DBC_POOL_AUTHORITY,
        dbc_config: look_config,
        dbc_pool: look_pool,
        dbc_base_vault: pda(&[b"token_vault", look_base.as_ref(), look_pool.as_ref()], &DBC_ID),
        dbc_quote_vault: pda(&[b"token_vault", look_quote.as_ref(), look_pool.as_ref()], &DBC_ID),
        dbc_event_authority: DBC_EVENT_AUTHORITY,
        dbc_program: DBC_ID,
        token_program: spl_token::ID,
        associated_token_program: anchor_spl::associated_token::ID,
        system_program: system_program::ID,
        event_authority: k.events,
        program: pl::ID,
    };
    json!({
        "inputs": {
            "keeper": keeper.to_string(),
            "bridge": bridge.to_string(),
            "second_nft_mint": second_nft.to_string(),
            "rent_payer": rent_payer.to_string(),
            "ticket": ticket.to_string(),
            "look_base": look_base.to_string(),
            "look_quote": look_quote.to_string(),
            "look_config": look_config.to_string(),
            "look_pool": look_pool.to_string(),
        },
        "data": {
            "report_backed": hex(&px::instruction::ReportBacked { amounts: vec![1_500_000, 7], seen: vec![3, 0], minted: vec![9, 2] }.data()),
            "settle": hex(&px::instruction::Settle { payout: 1_000_000 }.data()),
            "set_paused": hex(&px::instruction::SetPaused { paused: false }.data()),
            "halt": hex(&px::instruction::Halt {}.data()),
            "pay_queue": hex(&px::instruction::PayQueue { max: 1 }.data()),
            "release": hex(&px::instruction::Release {}.data()),
            "absorb": hex(&px::instruction::Absorb {}.data()),
            "send_to_bridge": hex(&px::instruction::SendToBridge { amount: 2_500_000_000 }.data()),
            "graduate": hex(&pl::instruction::Graduate {}.data()),
            "expire_launch": hex(&pl::instruction::ExpireLaunch {}.data()),
            "sweep_fees": hex(&pl::instruction::SweepFees {}.data()),
        },
        "accounts": {
            "report_backed": with(batch.to_account_metas(None), vec![w(k.market), r(k.ptoken), w(k.market), r(k.ptoken)]),
            "settle": metas(admin.to_account_metas(None)),
            "set_paused": with(batch.to_account_metas(None), vec![w(k.market)]),
            "halt": metas(halt.to_account_metas(None)),
            "pay_queue": with(pay.to_account_metas(None), triple.clone()),
            "release": with(release.to_account_metas(None), triple),
            "absorb": metas(absorb.to_account_metas(None)),
            "send_to_bridge": metas(send.to_account_metas(None)),
            "graduate_migrate": metas(graduate.to_account_metas(None)),
            "graduate_record": metas(graduate_record.to_account_metas(None)),
            "expire_launch": metas(expire.to_account_metas(None)),
            "sweep_fees": metas(sweep.to_account_metas(None)),
        },
    })
}

#[test]
fn the_client_fixture_matches_the_programs() {
    let v = vector();
    if std::env::var("WRITE_CLIENT_FIXTURE").as_deref() == Ok("1") {
        std::fs::write(FIXTURE, serde_json::to_string_pretty(&v).unwrap() + "\n").unwrap();
    }
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(FIXTURE).expect("client_v1.json missing")).unwrap();
    assert_eq!(on_disk, v, "client_v1.json is stale: rewrite it with WRITE_CLIENT_FIXTURE=1");
}

#[test]
fn pinned_meteora_addresses_are_their_pdas() {
    // meteora.rs pins these; the fixture's derived values must agree.
    assert_eq!(pda(&[b"pool_authority"], &DBC_ID), DBC_POOL_AUTHORITY);
    assert_eq!(pda(&[b"__event_authority"], &DBC_ID), DBC_EVENT_AUTHORITY);
    assert_eq!(pda(&[b"pool_authority"], &DAMM_ID), DAMM_POOL_AUTHORITY);
    assert_eq!(pda(&[b"__event_authority"], &DAMM_ID), DAMM_EVENT_AUTHORITY);
    assert_eq!(Pubkey::from_str(&quote_field("program_id")).unwrap(), px::ID);
}
