//! LiteSVM harness for `polypad_launch` against the real Meteora DBC, DAMM v2
//! and Metaplex programs dumped from mainnet (`tests/fixtures/fetch.sh`),
//! the real `Hv8L…` DAMM v2 config account, and the built exchange and launch
//! programs (`anchor build` first). Every instruction is built from the
//! programs' own Anchor client types (Meteora's from the vendored IDLs).

#![allow(dead_code)]

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program_option::COption;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::{pubkey, system_program, AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::spl_token;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_clock::Clock;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

pub use pl::cp_amm;
pub use pl::dynamic_bonding_curve as dbc;
pub use pl::error::LaunchError;
pub use pl::meteora::*;
pub use pl::state::*;
pub use polypad_exchange as px;
pub use polypad_launch as pl;
pub use px::quote::{Quote, BUY, SELL};

/// 2027-01-15T08:00:00Z.
pub const T0: i64 = 1_800_000_000;
pub const USDC: u64 = 1_000_000;
pub const SHARE: u64 = 1_000_000;
pub const ALT_PROGRAM: Pubkey = pubkey!("AddressLookupTab1e1111111111111111111111111");
pub const COMPUTE_BUDGET: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");
pub const ED25519: Pubkey = pubkey!("Ed25519SigVerify111111111111111111111111111");
pub const INSTRUCTIONS: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");
/// Other DAMM v2 configs only DBC may use (mainnet, 2026-10-13).
pub const DAMM_CONFIG_2PCT: Pubkey = pubkey!("2c4cYd4reUYVRAB9kUUkrq55VPyy2FNQ3FDL4o12JXmq");
pub const DAMM_CONFIG_CUSTOM: Pubkey = pubkey!("A8gMrEPJkacWkcb3DGwtJwTe16HktSEfvwtuDh2MCtck");

pub fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

pub fn pid(n: u8) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[0] = 0x5a;
    id[31] = n;
    id
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

pub struct User {
    pub kp: Keypair,
    pub usdc: Pubkey,
}

impl User {
    pub fn key(&self) -> Pubkey {
        self.kp.pubkey()
    }
}

/// A launched (or about to be launched) coin's addresses.
#[derive(Clone, Debug)]
pub struct Coin {
    pub pid: [u8; 32],
    pub market: Pubkey,
    pub ptoken: Pubkey,
    pub held: Pubkey,
    pub base_mint: Pubkey,
    pub config: Pubkey,
    pub pool: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub launch: Pubkey,
}

impl Coin {
    pub fn damm_pool(&self) -> Pubkey {
        damm_pool_address(&DAMM_CONFIG, &self.base_mint, &self.ptoken)
    }
}

/// What one transaction cost: compute units, wire size, accounts, logs.
#[derive(Debug, Clone)]
pub struct Sent {
    pub cu: u64,
    pub size: usize,
    pub accounts: usize,
    pub logs: Vec<String>,
}

impl Sent {
    /// Compute units of each top-level instruction, from the runtime's
    /// "consumed" lines at invoke depth 1.
    pub fn per_instruction(&self) -> Vec<(String, u64)> {
        let mut out = vec![];
        let mut depth = 0;
        for l in &self.logs {
            if let Some(rest) = l.strip_prefix("Program ") {
                if rest.contains(" invoke [") {
                    depth += 1;
                } else if rest.contains(" consumed ") {
                    if depth == 1 {
                        let mut it = rest.split_whitespace();
                        let prog = it.next().unwrap_or("").to_string();
                        let n = it.nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                        out.push((prog, n));
                    }
                } else if rest.ends_with(" success") || rest.contains(" failed") {
                    depth -= 1;
                }
            }
        }
        out
    }
}

pub struct LaunchArgs {
    pub holders_bps: u16,
    /// USDC the creator spends on a first buy in the launch transaction (0 = none).
    pub first_buy: u64,
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

impl Default for LaunchArgs {
    fn default() -> Self {
        LaunchArgs {
            holders_bps: 0,
            first_buy: 0,
            name: "Will it rain".into(),
            symbol: "RAIN".into(),
            uri: "https://polypad.fun/c/rain.json".into(),
        }
    }
}

pub type Res<T = Sent> = Result<T, TransactionError>;

pub fn launch_code(e: LaunchError) -> u32 {
    e.into()
}

#[track_caller]
pub fn expect_err<T: std::fmt::Debug>(r: Res<T>, e: LaunchError) {
    match r {
        Err(TransactionError::InstructionError(_, InstructionError::Custom(c))) if c == launch_code(e) => {}
        other => panic!("expected {e:?} ({}), got {other:?}", launch_code(e)),
    }
}

/// Any custom error at instruction `ix` (Anchor constraint codes, Meteora's).
#[track_caller]
pub fn expect_custom<T: std::fmt::Debug>(r: Res<T>, ix: u8, want: u32) {
    match r {
        Err(TransactionError::InstructionError(i, InstructionError::Custom(c))) if c == want && i == ix => {}
        other => panic!("expected custom {want} at {ix}, got {other:?}"),
    }
}

#[track_caller]
pub fn expect_fail<T: std::fmt::Debug>(r: Res<T>) -> TransactionError {
    match r {
        Err(e) => e,
        Ok(v) => panic!("expected failure, got {v:?}"),
    }
}

pub struct Env {
    pub svm: LiteSVM,
    pub owner: Keypair,
    pub keeper: Keypair,
    pub platform: Keypair,
    pub holder_payout: Keypair,
    pub signer: SigningKey,
    pub usdc_auth: Keypair,
    pub usdc_mint: Pubkey,
    pub now: i64,
    pub nonce: std::cell::Cell<u64>,
    pub alt: Option<AddressLookupTableAccount>,
    /// When set, every successful transaction is kept in `recorded` as RPC
    /// `getTransaction` returns it (`rpc_json`), for the indexer's fixture.
    pub record: bool,
    pub recorded: Vec<serde_json::Value>,
}

impl Env {
    /// Programs loaded, the exchange and the factory initialized, the factory
    /// set as the exchange's factory signer, risk caps off.
    pub fn new() -> Env {
        let mut svm = LiteSVM::new();
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/");
        for (id, path) in [
            (px::ID, format!("{target}polypad_exchange.so")),
            (pl::ID, format!("{target}polypad_launch.so")),
            (DBC_ID, fixture("dbc.so")),
            (DAMM_ID, fixture("cp_amm.so")),
            (METADATA_PROGRAM, fixture("mpl_token_metadata.so")),
        ] {
            let so = std::fs::read(&path)
                .unwrap_or_else(|_| panic!("{path} missing: run `anchor build --ignore-keys` and tests/fixtures/fetch.sh"));
            svm.add_program(id, &so).unwrap();
        }
        let owner = Keypair::new();
        // LiteSVM deploys with no upgrade authority; make the owner the
        // authority of ours, as a real deploy would, so it may initialize.
        for id in [px::ID, pl::ID] {
            let pd = pda(&[id.as_ref()], &anchor_lang::solana_program::bpf_loader_upgradeable::ID);
            let mut acc = svm.get_account(&pd).unwrap();
            acc.data[12] = 1;
            acc.data[13..45].copy_from_slice(owner.pubkey().as_ref());
            svm.set_account(pd, acc).unwrap();
        }
        // The real `Hv8L…` config, and two other configs only DBC may use
        // (2% static, customizable), as fetched from mainnet.
        for (key, file) in [(DAMM_CONFIG, "hv8l.json"), (DAMM_CONFIG_2PCT, "damm_config_2pct.json"), (DAMM_CONFIG_CUSTOM, "damm_config_custom.json")] {
            let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixture(file)).unwrap()).unwrap();
            let a = &j["account"];
            let data = base64::engine::general_purpose::STANDARD.decode(a["data"][0].as_str().unwrap()).unwrap();
            let account = Account {
                lamports: a["lamports"].as_u64().unwrap(),
                data,
                owner: a["owner"].as_str().unwrap().parse().unwrap(),
                executable: false,
                rent_epoch: 0,
            };
            svm.set_account(key, account).unwrap();
        }

        let (keeper, platform, holder_payout, usdc_auth) = (Keypair::new(), Keypair::new(), Keypair::new(), Keypair::new());
        for k in [&owner, &keeper, &platform, &holder_payout, &usdc_auth] {
            svm.airdrop(&k.pubkey(), 1_000_000_000_000).unwrap();
        }
        // DBC's pool authority advances the DAMM v2 rent at migration and is
        // repaid by the migrator in the same instruction (`flash_rent`); on
        // both clusters it holds SOL for that (69 SOL mainnet, 2026-10-13).
        svm.airdrop(&DBC_POOL_AUTHORITY, 10_000_000_000).unwrap();
        let usdc_mint = Pubkey::new_unique();
        let mut env = Env {
            svm,
            owner,
            keeper,
            platform,
            holder_payout,
            signer: SigningKey::from_bytes(&[42; 32]),
            usdc_auth,
            usdc_mint,
            now: T0,
            nonce: std::cell::Cell::new(0),
            alt: None,
            record: false,
            recorded: vec![],
        };
        env.set_mint(usdc_mint, 6, Some(env.usdc_auth.pubkey()));
        env.warp_to(T0);
        env.init_exchange();
        env.init_factory();
        env.alt = Some(env.create_alt(&env.static_accounts()));
        env
    }

    fn init_exchange(&mut self) {
        let o = self.owner.insecure_clone();
        let accounts = px::accounts::Initialize {
            payer: o.pubkey(),
            config: self.ex_config(),
            vault: self.float(),
            usdc_mint: self.usdc_mint,
            exchange_program: px::ID,
            program_data: pda(&[px::ID.as_ref()], &anchor_lang::solana_program::bpf_loader_upgradeable::ID),
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.ex_events(),
            program: px::ID,
        };
        let data = px::instruction::Initialize { owner: o.pubkey(), keeper: self.keeper.pubkey(), signer: self.signer_key() }.data();
        self.send(&[Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data }], &o, &[]).unwrap();
        let bridge = Pubkey::new_unique();
        self.set_token(bridge, self.usdc_mint, Pubkey::new_unique(), 0);
        let keeper = self.keeper.pubkey();
        self.ex_owner_ix(px::instruction::SetRoles { keeper, factory: self.factory(), bridge_deposit: bridge }).unwrap();
        self.ex_owner_ix(px::instruction::SetMaxTotalRisk { cap: u64::MAX }).unwrap();
        self.ex_owner_ix(px::instruction::SetParams {
            buy_spread_bps: 25,
            sell_spread_bps: 25,
            min_price: 1_000,
            max_price: 999_000,
            default_max_risk: u64::MAX,
        })
        .unwrap();
    }

    fn init_factory(&mut self) {
        let o = self.owner.insecure_clone();
        let accounts = pl::accounts::Initialize {
            payer: o.pubkey(),
            factory: self.factory(),
            fee_vault: self.fee_vault(),
            launch_program: pl::ID,
            program_data: pda(&[pl::ID.as_ref()], &anchor_lang::solana_program::bpf_loader_upgradeable::ID),
            system_program: system_program::ID,
            event_authority: self.events(),
            program: pl::ID,
        };
        let data = pl::instruction::Initialize {
            owner: o.pubkey(),
            keeper: self.keeper.pubkey(),
            platform: self.platform.pubkey(),
            holder_payout: self.holder_payout.pubkey(),
        }
        .data();
        self.send(&[Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data }], &o, &[]).unwrap();
    }

    pub fn ex_owner_ix<T: InstructionData>(&mut self, data: T) -> Res {
        let o = self.owner.insecure_clone();
        let accounts =
            px::accounts::OwnerOnly { owner: o.pubkey(), config: self.ex_config(), event_authority: self.ex_events(), program: px::ID };
        self.send(&[Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: data.data() }], &o, &[])
    }

    pub fn owner_ix<T: InstructionData>(&mut self, who: &Keypair, data: T) -> Res {
        let accounts =
            pl::accounts::OwnerOnly { owner: who.pubkey(), factory: self.factory(), event_authority: self.events(), program: pl::ID };
        self.send(&[Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: data.data() }], who, &[])
    }

    /* ------------------------------------------------------- addresses */

    pub fn ex_config(&self) -> Pubkey {
        pda(&[px::state::CONFIG_SEED], &px::ID)
    }
    pub fn float(&self) -> Pubkey {
        pda(&[px::state::VAULT_SEED], &px::ID)
    }
    pub fn ex_events(&self) -> Pubkey {
        pda(&[b"__event_authority"], &px::ID)
    }
    pub fn events(&self) -> Pubkey {
        pda(&[b"__event_authority"], &pl::ID)
    }
    pub fn factory(&self) -> Pubkey {
        pda(&[FACTORY_SEED], &pl::ID)
    }
    pub fn fee_vault(&self) -> Pubkey {
        pda(&[FEE_VAULT_SEED], &pl::ID)
    }
    pub fn signer_key(&self) -> Pubkey {
        Pubkey::new_from_array(self.signer.verifying_key().to_bytes())
    }

    pub fn coin(&self, id: [u8; 32], base_mint: Pubkey) -> Coin {
        let ptoken = pda(&[px::state::PTOKEN_SEED, &id], &px::ID);
        let config = pda(&[DBC_CONFIG_SEED, base_mint.as_ref()], &pl::ID);
        let pool = pool_address(&config, &base_mint, &ptoken);
        Coin {
            pid: id,
            market: pda(&[px::state::MARKET_SEED, &id], &px::ID),
            ptoken,
            held: pda(&[px::state::HELD_SEED, &id], &px::ID),
            base_mint,
            config,
            pool,
            base_vault: pda(&[b"token_vault", base_mint.as_ref(), pool.as_ref()], &DBC_ID),
            quote_vault: pda(&[b"token_vault", ptoken.as_ref(), pool.as_ref()], &DBC_ID),
            launch: pda(&[LAUNCH_SEED, base_mint.as_ref()], &pl::ID),
        }
    }

    /// Accounts every launch, trade and migration shares: one lookup table.
    pub fn static_accounts(&self) -> Vec<Pubkey> {
        vec![
            px::ID,
            pl::ID,
            DBC_ID,
            DAMM_ID,
            METADATA_PROGRAM,
            spl_token::ID,
            TOKEN_2022,
            anchor_spl::associated_token::ID,
            system_program::ID,
            ED25519,
            COMPUTE_BUDGET,
            INSTRUCTIONS,
            self.ex_config(),
            self.float(),
            self.usdc_mint,
            self.ex_events(),
            self.factory(),
            self.fee_vault(),
            self.events(),
            DBC_POOL_AUTHORITY,
            DBC_EVENT_AUTHORITY,
            DAMM_POOL_AUTHORITY,
            DAMM_EVENT_AUTHORITY,
            DAMM_CONFIG,
        ]
    }

    /* ----------------------------------------------------------- chain */

    pub fn warp_to(&mut self, t: i64) {
        self.now = t;
        let mut c: Clock = self.svm.get_sysvar();
        c.unix_timestamp = t;
        c.slot += 1;
        self.svm.set_sysvar(&c);
    }

    pub fn warp(&mut self, secs: i64) {
        self.warp_to(self.now + secs);
    }

    fn signers<'a>(payer: &'a Keypair, extra: &[&'a Keypair]) -> Vec<&'a Keypair> {
        let mut v: Vec<&Keypair> = vec![payer];
        for k in extra {
            if !v.iter().any(|s| s.pubkey() == k.pubkey()) {
                v.push(k);
            }
        }
        v
    }

    fn result(&mut self, tx: VersionedTransaction, size: usize, accounts: usize) -> Res {
        let keep = self.record.then(|| {
            let keys = self.account_keys(&tx);
            let pre = self.token_balances(&keys);
            (tx.clone(), keys, pre)
        });
        let r = self.svm.send_transaction(tx);
        self.svm.expire_blockhash();
        match r {
            Ok(m) => {
                if let Some((tx, keys, pre)) = keep {
                    let post = self.token_balances(&keys);
                    let inner: Vec<Vec<(u8, Vec<u8>, Vec<u8>, u8)>> = m
                        .inner_instructions
                        .iter()
                        .map(|l| l.iter().map(|i| (i.instruction.program_id_index, i.instruction.accounts.clone(), i.instruction.data.clone(), i.stack_height)).collect())
                        .collect();
                    let j = self.rpc_json(&tx, &keys, &inner, m.fee, m.compute_units_consumed, pre, post);
                    self.recorded.push(j);
                }
                Ok(Sent { cu: m.compute_units_consumed, size, accounts, logs: m.logs })
            }
            Err(e) => {
                if std::env::var("PL_LOGS").is_ok() {
                    eprintln!("{}", e.meta.logs.join("\n"));
                }
                Err(e.err)
            }
        }
    }

    /// A legacy transaction with a 1.4M compute limit in front.
    pub fn send(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Res {
        let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
        all.extend_from_slice(ixs);
        let signers = Env::signers(payer, extra);
        let tx = Transaction::new_signed_with_payer(&all, Some(&payer.pubkey()), &signers, self.svm.latest_blockhash());
        let size = bincode::serialize(&tx).unwrap().len();
        let accounts = tx.message.account_keys.len();
        self.result(tx.into(), size, accounts)
    }

    /// A v0 transaction over the shared lookup table, as the API sends them.
    pub fn send_v0(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Res {
        let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
        all.extend_from_slice(ixs);
        let alts = [self.alt.clone().unwrap()];
        let msg = v0::Message::try_compile(&payer.pubkey(), &all, &alts, self.svm.latest_blockhash()).unwrap();
        let accounts = msg.account_keys.len()
            + msg.address_table_lookups.iter().map(|l| l.writable_indexes.len() + l.readonly_indexes.len()).sum::<usize>();
        let signers = Env::signers(payer, extra);
        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &signers).unwrap();
        let size = bincode::serialize(&tx).unwrap().len();
        self.result(tx, size, accounts)
    }

    /* -------------------------------------------------------- recording */

    /// Every account a transaction names: the static keys, then the lookup
    /// table's writable and read-only ones, the order instruction indexes use.
    pub fn account_keys(&self, tx: &VersionedTransaction) -> (Vec<Pubkey>, Vec<Pubkey>, Vec<Pubkey>) {
        let (mut writable, mut readonly) = (vec![], vec![]);
        for l in tx.message.address_table_lookups().unwrap_or(&[]) {
            let table = self.alt.as_ref().filter(|a| a.key == l.account_key).expect("an unknown lookup table");
            writable.extend(l.writable_indexes.iter().map(|i| table.addresses[*i as usize]));
            readonly.extend(l.readonly_indexes.iter().map(|i| table.addresses[*i as usize]));
        }
        (tx.message.static_account_keys().to_vec(), writable, readonly)
    }

    /// The SPL token accounts among `keys`, as RPC's `pre/postTokenBalances`.
    fn token_balances(&self, keys: &(Vec<Pubkey>, Vec<Pubkey>, Vec<Pubkey>)) -> Vec<serde_json::Value> {
        let all: Vec<&Pubkey> = keys.0.iter().chain(&keys.1).chain(&keys.2).collect();
        let mut out = vec![];
        for (i, k) in all.iter().enumerate() {
            let Some(a) = self.svm.get_account(k) else { continue };
            if a.owner != spl_token::ID || a.data.len() != spl_token::state::Account::LEN {
                continue;
            }
            let t = spl_token::state::Account::unpack(&a.data).unwrap();
            let decimals = spl_token::state::Mint::unpack(&self.svm.get_account(&t.mint).unwrap().data).unwrap().decimals;
            out.push(serde_json::json!({
                "accountIndex": i,
                "mint": t.mint.to_string(),
                "owner": t.owner.to_string(),
                "programId": spl_token::ID.to_string(),
                "uiTokenAmount": { "amount": t.amount.to_string(), "decimals": decimals },
            }));
        }
        out
    }

    /// A transaction as RPC `getTransaction` returns it with `encoding:
    /// "json"` (instruction data base58, inner instructions with their stack
    /// height), at the current slot and clock.
    #[allow(clippy::too_many_arguments)]
    fn rpc_json(
        &self,
        tx: &VersionedTransaction,
        keys: &(Vec<Pubkey>, Vec<Pubkey>, Vec<Pubkey>),
        inner: &[Vec<(u8, Vec<u8>, Vec<u8>, u8)>],
        fee: u64,
        cu: u64,
        pre: Vec<serde_json::Value>,
        post: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        use serde_json::json;
        let b58 = |d: &[u8]| bs58::encode(d).into_string();
        let strs = |v: &[Pubkey]| v.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        let clock: Clock = self.svm.get_sysvar();
        let h = tx.message.header();
        let lookups: Vec<_> = tx
            .message
            .address_table_lookups()
            .unwrap_or(&[])
            .iter()
            .map(|l| json!({ "accountKey": l.account_key.to_string(), "writableIndexes": l.writable_indexes, "readonlyIndexes": l.readonly_indexes }))
            .collect();
        let instructions: Vec<_> = tx
            .message
            .instructions()
            .iter()
            .map(|ix| json!({ "programIdIndex": ix.program_id_index, "accounts": ix.accounts, "data": b58(&ix.data), "stackHeight": null }))
            .collect();
        let inner: Vec<_> = inner
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, l)| {
                let ixs: Vec<_> =
                    l.iter().map(|(p, a, d, h)| json!({ "programIdIndex": p, "accounts": a, "data": b58(d), "stackHeight": h })).collect();
                json!({ "index": i, "instructions": ixs })
            })
            .collect();
        let v0 = matches!(tx.message, VersionedMessage::V0(_));
        json!({
            "slot": clock.slot,
            "blockTime": self.now,
            "version": if v0 { json!(0) } else { json!("legacy") },
            "transaction": {
                "signatures": tx.signatures.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "message": {
                    "accountKeys": strs(&keys.0),
                    "header": {
                        "numRequiredSignatures": h.num_required_signatures,
                        "numReadonlySignedAccounts": h.num_readonly_signed_accounts,
                        "numReadonlyUnsignedAccounts": h.num_readonly_unsigned_accounts,
                    },
                    "recentBlockhash": tx.message.recent_blockhash().to_string(),
                    "instructions": instructions,
                    "addressTableLookups": lookups,
                },
            },
            "meta": {
                "err": null,
                "fee": fee,
                "computeUnitsConsumed": cu,
                "loadedAddresses": { "writable": strs(&keys.1), "readonly": strs(&keys.2) },
                "innerInstructions": inner,
                "preTokenBalances": pre,
                "postTokenBalances": post,
            },
        })
    }

    pub fn create_alt(&mut self, addresses: &[Pubkey]) -> AddressLookupTableAccount {
        let key = Pubkey::new_unique();
        // `AddressLookupTable` state: ProgramState::LookupTable tag, then
        // LookupTableMeta (deactivation slot, last extended slot and index,
        // Option<authority>, padding), then the addresses.
        let mut data = 1u32.to_le_bytes().to_vec();
        data.extend(u64::MAX.to_le_bytes());
        data.extend(0u64.to_le_bytes());
        data.push(0);
        data.push(1);
        data.extend(self.owner.pubkey().as_ref());
        data.extend([0u8; 2]);
        assert_eq!(data.len(), 56);
        for a in addresses {
            data.extend(a.as_ref());
        }
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm.set_account(key, Account { lamports, data, owner: ALT_PROGRAM, executable: false, rent_epoch: 0 }).unwrap();
        AddressLookupTableAccount { key, addresses: addresses.to_vec() }
    }

    /* ---------------------------------------------------------- tokens */

    pub fn set_mint(&mut self, addr: Pubkey, decimals: u8, authority: Option<Pubkey>) {
        let m = spl_token::state::Mint {
            mint_authority: authority.map_or(COption::None, COption::Some),
            supply: 0,
            decimals,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        let mut data = vec![0u8; spl_token::state::Mint::LEN];
        m.pack_into_slice(&mut data);
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm.set_account(addr, Account { lamports, data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
    }

    pub fn set_token(&mut self, addr: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64) {
        let a = spl_token::state::Account {
            mint,
            owner,
            amount,
            delegate: COption::None,
            state: spl_token::state::AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        let mut data = vec![0u8; spl_token::state::Account::LEN];
        a.pack_into_slice(&mut data);
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm.set_account(addr, Account { lamports, data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
    }

    pub fn exists(&self, addr: &Pubkey) -> bool {
        self.svm.get_account(addr).is_some_and(|a| a.lamports > 0)
    }

    pub fn balance(&self, addr: &Pubkey) -> u64 {
        match self.svm.get_account(addr) {
            Some(a) if a.lamports > 0 => spl_token::state::Account::unpack(&a.data[..165]).unwrap().amount,
            _ => 0,
        }
    }

    pub fn supply(&self, mint: &Pubkey) -> u64 {
        spl_token::state::Mint::unpack(&self.svm.get_account(mint).unwrap().data[..82]).unwrap().supply
    }

    pub fn user(&mut self, usdc: u64) -> User {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
        let acct = Pubkey::new_unique();
        let (mint, owner) = (self.usdc_mint, kp.pubkey());
        self.set_token(acct, mint, owner, usdc);
        User { kp, usdc: acct }
    }

    /// `owner`'s ATA for `mint`, created empty if missing (state set directly).
    pub fn ata(&mut self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let a = get_associated_token_address(owner, mint);
        if !self.exists(&a) {
            self.set_token(a, *mint, *owner, 0);
        }
        a
    }

    pub fn ata_create_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
        Instruction {
            program_id: anchor_spl::associated_token::ID,
            accounts: vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(get_associated_token_address(owner, mint), false),
                AccountMeta::new_readonly(*owner, false),
                AccountMeta::new_readonly(*mint, false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: vec![1],
        }
    }

    /* ----------------------------------------------------------- state */

    pub fn launch_of(&self, c: &Coin) -> Launch {
        Launch::try_deserialize(&mut &self.svm.get_account(&c.launch).unwrap().data[..]).unwrap()
    }

    pub fn factory_state(&self) -> Factory {
        Factory::try_deserialize(&mut &self.svm.get_account(&self.factory()).unwrap().data[..]).unwrap()
    }

    pub fn owed(&self, mint: &Pubkey, payee: &Pubkey) -> u64 {
        self.owed_at(OWED_SEED, mint, payee)
    }

    pub fn platform_owed(&self, mint: &Pubkey, platform: &Pubkey) -> u64 {
        self.owed_at(PLATFORM_OWED_SEED, mint, platform)
    }

    fn owed_at(&self, seed: &[u8], mint: &Pubkey, payee: &Pubkey) -> u64 {
        match self.svm.get_account(&pda(&[seed, mint.as_ref(), payee.as_ref()], &pl::ID)) {
            Some(a) if a.lamports > 0 => Owed::try_deserialize(&mut &a.data[..]).unwrap().amount,
            _ => 0,
        }
    }

    pub fn ex_state(&self) -> px::state::Config {
        px::state::Config::try_deserialize(&mut &self.svm.get_account(&self.ex_config()).unwrap().data[..]).unwrap()
    }

    pub fn pool_config(&self, c: &Coin) -> dbc::accounts::PoolConfig {
        dbc::accounts::PoolConfig::try_deserialize(&mut &self.svm.get_account(&c.config).unwrap().data[..]).unwrap()
    }

    pub fn curve_pool(&self, pool: &Pubkey) -> dbc::accounts::VirtualPool {
        dbc::accounts::VirtualPool::try_deserialize(&mut &self.svm.get_account(pool).unwrap().data[..]).unwrap()
    }

    pub fn damm_pool_state(&self, c: &Coin) -> cp_amm::accounts::Pool {
        cp_amm::accounts::Pool::try_deserialize(&mut &self.svm.get_account(&c.damm_pool()).unwrap().data[..]).unwrap()
    }

    pub fn position_state(&self, p: &Pubkey) -> cp_amm::accounts::Position {
        cp_amm::accounts::Position::try_deserialize(&mut &self.svm.get_account(p).unwrap().data[..]).unwrap()
    }

    /* ---------------------------------------------------------- quotes */

    pub fn quote(&self, id: [u8; 32], side: u8, price: u64, max_amount: u64) -> Quote {
        let n = self.nonce.get();
        self.nonce.set(n + 1);
        Quote { position_id: id, side, price, max_amount: max_amount + n, valid_until: self.now + 15 }
    }

    pub fn ed25519_ix_by(key: &SigningKey, message: &[u8]) -> Instruction {
        let sig = key.sign(message).to_bytes();
        let pk_off = 16u16;
        let sig_off = pk_off + 32;
        let msg_off = sig_off + 64;
        let mut d = vec![1u8, 0];
        for v in [sig_off, u16::MAX, pk_off, u16::MAX, msg_off, message.len() as u16, u16::MAX] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&key.verifying_key().to_bytes());
        d.extend_from_slice(&sig);
        d.extend_from_slice(message);
        Instruction { program_id: ED25519, accounts: vec![], data: d }
    }

    pub fn ed25519_ix(&self, q: &Quote) -> Instruction {
        Env::ed25519_ix_by(&self.signer, &q.message(&px::ID))
    }

    /* ---------------------------------------------------------- launch */

    pub fn launch_ix(&self, creator: &User, c: &Coin, q: &Quote, a: &LaunchArgs) -> Instruction {
        let creator_usdc = creator.usdc;
        let creator = &creator.key();
        let buyer_usdc = if a.first_buy > 0 { Some(creator_usdc) } else { None };
        let fb = a.first_buy > 0;
        let some = |k: Pubkey| if fb { Some(k) } else { None };
        let accounts = pl::accounts::LaunchCoin {
            creator: *creator,
            factory: self.factory(),
            fee_vault: self.fee_vault(),
            exchange_config: self.ex_config(),
            market: c.market,
            ptoken_mint: c.ptoken,
            held: c.held,
            dbc_config: c.config,
            base_mint: c.base_mint,
            launch: c.launch,
            instructions: INSTRUCTIONS,
            exchange_event_authority: self.ex_events(),
            exchange_program: px::ID,
            dbc_event_authority: DBC_EVENT_AUTHORITY,
            dbc_program: DBC_ID,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            buyer_usdc,
            creator_ptoken: some(get_associated_token_address(creator, &c.ptoken)),
            usdc_vault: some(self.float()),
            usdc_mint: some(self.usdc_mint),
            fill: some(pda(&[px::state::FILL_SEED, &q.fill_seed()], &px::ID)),
            associated_token_program: some(anchor_spl::associated_token::ID),
            event_authority: self.events(),
            program: pl::ID,
        };
        Instruction {
            program_id: pl::ID,
            accounts: accounts.to_account_metas(None),
            data: pl::instruction::Launch { quote: *q, holders_bps: a.holders_bps, first_buy: a.first_buy, first_buy_min_out: 0 }.data(),
        }
    }

    pub fn pool_init_ix(&self, creator: &Pubkey, payer: &Pubkey, c: &Coin, a: &LaunchArgs) -> Instruction {
        let accounts = dbc::client::accounts::InitializeVirtualPoolWithSplToken {
            config: c.config,
            pool_authority: DBC_POOL_AUTHORITY,
            creator: *creator,
            base_mint: c.base_mint,
            quote_mint: c.ptoken,
            pool: c.pool,
            base_vault: c.base_vault,
            quote_vault: c.quote_vault,
            mint_metadata: pda(&[b"metadata", METADATA_PROGRAM.as_ref(), c.base_mint.as_ref()], &METADATA_PROGRAM),
            metadata_program: METADATA_PROGRAM,
            payer: *payer,
            token_quote_program: spl_token::ID,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: DBC_EVENT_AUTHORITY,
            program: DBC_ID,
        };
        let params = dbc::types::InitializePoolParameters { name: a.name.clone(), symbol: a.symbol.clone(), uri: a.uri.clone() };
        Instruction {
            program_id: DBC_ID,
            accounts: accounts.to_account_metas(None),
            data: dbc::client::args::InitializeVirtualPoolWithSplToken { params }.data(),
        }
    }

    /// DBC `swap2` for `user` on the coin's curve. `buy`: pToken in, else coin in.
    #[allow(clippy::too_many_arguments)]
    pub fn curve_swap_ix(&self, user: &Pubkey, c: &Coin, buy: bool, amount_0: u64, amount_1: u64, mode: u8, first: bool) -> Instruction {
        let p = get_associated_token_address(user, &c.ptoken);
        let k = get_associated_token_address(user, &c.base_mint);
        let (input, output) = if buy { (p, k) } else { (k, p) };
        let accounts = dbc::client::accounts::Swap2 {
            pool_authority: DBC_POOL_AUTHORITY,
            config: c.config,
            pool: c.pool,
            input_token_account: input,
            output_token_account: output,
            base_vault: c.base_vault,
            quote_vault: c.quote_vault,
            base_mint: c.base_mint,
            quote_mint: c.ptoken,
            payer: *user,
            token_base_program: spl_token::ID,
            token_quote_program: spl_token::ID,
            referral_token_account: None,
            event_authority: DBC_EVENT_AUTHORITY,
            program: DBC_ID,
        };
        let mut metas = accounts.to_account_metas(None);
        if first {
            // The instructions sysvar lets DBC see the pool creation in this
            // transaction and charge the first swap the minimum fee.
            metas.push(AccountMeta::new_readonly(INSTRUCTIONS, false));
        }
        let params = dbc::types::SwapParameters2 { amount_0, amount_1, swap_mode: mode };
        Instruction { program_id: DBC_ID, accounts: metas, data: dbc::client::args::Swap2 { params }.data() }
    }

    /// pToken minted for `usdc_in` at `price` (the exchange's own math).
    pub fn minted(&self, price: u64, usdc_in: u64) -> u64 {
        px::math::mint_out(price, usdc_in, self.ex_state().buy_spread_bps).unwrap()
    }

    /// The launch as the API builds it: one transaction without a first
    /// buy; with one, a two-transaction bundle (launch, then pool creation and
    /// the creator's swap, which DBC charges the curve fee only when both are
    /// top-level instructions of one transaction).
    pub fn launch_txs(&self, u: &User, c: &Coin, q: &Quote, a: &LaunchArgs) -> Vec<Vec<Instruction>> {
        let creator = &u.key();
        let (ed, launch, init) = (self.ed25519_ix(q), self.launch_ix(u, c, q, a), self.pool_init_ix(creator, creator, c, a));
        if a.first_buy == 0 {
            return vec![vec![ed, launch, init]];
        }
        let swap = self.curve_swap_ix(creator, c, true, self.minted(q.price, a.first_buy), 0, 1, true);
        vec![vec![ed, launch], vec![init, Env::ata_create_ix(creator, creator, &c.base_mint), swap]]
    }

    /// Launch a coin on market `id` at `price`, as `creator`.
    pub fn launch(&mut self, creator: &User, id: [u8; 32], price: u64, a: &LaunchArgs) -> Res<(Coin, Vec<Sent>)> {
        let mint = Keypair::new();
        let c = self.coin(id, mint.pubkey());
        let q = self.quote(id, BUY, price, a.first_buy.max(1));
        let kp = creator.kp.insecure_clone();
        let mut sent = vec![];
        for tx in self.launch_txs(creator, &c, &q, a) {
            sent.push(self.send_v0(&tx, &kp, &[&mint])?);
        }
        Ok((c, sent))
    }

    /* ----------------------------------------------------------- trade */

    pub fn mint_ix(&self, u: &User, c: &Coin, usdc_in: u64, q: &Quote) -> Instruction {
        let accounts = px::accounts::MintQuoted {
            buyer: u.key(),
            config: self.ex_config(),
            market: c.market,
            ptoken_mint: c.ptoken,
            buyer_usdc: u.usdc,
            to: get_associated_token_address(&u.key(), &c.ptoken),
            vault: self.float(),
            usdc_mint: self.usdc_mint,
            fill: pda(&[px::state::FILL_SEED, &q.fill_seed()], &px::ID),
            instructions: INSTRUCTIONS,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.ex_events(),
            program: px::ID,
        };
        Instruction {
            program_id: px::ID,
            accounts: accounts.to_account_metas(None),
            data: px::instruction::Mint { usdc_in, min_out: 0, quote: *q }.data(),
        }
    }

    pub fn redeem_ix(&self, u: &User, c: &Coin, amount_in: u64, q: &Quote) -> Instruction {
        let cfg = self.ex_state();
        let accounts = px::accounts::RedeemQuoted {
            seller: u.key(),
            config: self.ex_config(),
            market: c.market,
            ptoken_mint: c.ptoken,
            seller_ptoken: get_associated_token_address(&u.key(), &c.ptoken),
            held: c.held,
            to: u.usdc,
            vault: self.float(),
            usdc_mint: self.usdc_mint,
            fill: pda(&[px::state::FILL_SEED, &q.fill_seed()], &px::ID),
            instructions: INSTRUCTIONS,
            new_claim: Some(pda(&[px::state::CLAIM_SEED, &cfg.claim_tail.to_le_bytes()], &px::ID)),
            new_ticket: Some(pda(&[px::state::DELAYED_SEED, &cfg.delayed_next.to_le_bytes()], &px::ID)),
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.ex_events(),
            program: px::ID,
        };
        Instruction {
            program_id: px::ID,
            accounts: accounts.to_account_metas(None),
            data: px::instruction::Redeem { amount_in, min_out: 0, quote: *q }.data(),
        }
    }

    /// USDC → pToken → coin on the curve, one transaction:
    /// ed25519, exchange `mint`, DBC `swap2` (PartialFill).
    pub fn buy(&mut self, u: &User, c: &Coin, usdc_in: u64, price: u64) -> Res {
        self.ata(&u.key(), &c.ptoken);
        self.ata(&u.key(), &c.base_mint);
        let q = self.quote(c.pid, BUY, price, usdc_in);
        let shares = self.minted(price, usdc_in);
        let ixs = vec![self.ed25519_ix(&q), self.mint_ix(u, c, usdc_in, &q), self.curve_swap_ix(&u.key(), c, true, shares, 0, 1, false)];
        let kp = u.kp.insecure_clone();
        self.send_v0(&ixs, &kp, &[])
    }

    /// coin → exactly `shares` pToken → USDC, one transaction: DBC `swap2`
    /// (ExactOut), ed25519, exchange `redeem`.
    pub fn sell(&mut self, u: &User, c: &Coin, shares: u64, max_coins: u64, price: u64) -> Res {
        self.ata(&u.key(), &c.ptoken);
        let q = self.quote(c.pid, SELL, price, shares);
        let ixs = vec![
            self.curve_swap_ix(&u.key(), c, false, shares, max_coins, 2, false),
            self.ed25519_ix(&q),
            self.redeem_ix(u, c, shares, &q),
        ];
        let kp = u.kp.insecure_clone();
        self.send_v0(&ixs, &kp, &[])
    }

    /// DAMM v2 `swap2` on the graduated pool (ExactIn).
    pub fn pool_swap_ix(&self, user: &Pubkey, c: &Coin, buy: bool, amount_in: u64) -> Instruction {
        let pool = c.damm_pool();
        let p = get_associated_token_address(user, &c.ptoken);
        let k = get_associated_token_address(user, &c.base_mint);
        let (input, output) = if buy { (p, k) } else { (k, p) };
        let accounts = cp_amm::client::accounts::Swap2 {
            pool_authority: DAMM_POOL_AUTHORITY,
            pool,
            input_token_account: input,
            output_token_account: output,
            token_a_vault: pda(&[b"token_vault", c.base_mint.as_ref(), pool.as_ref()], &DAMM_ID),
            token_b_vault: pda(&[b"token_vault", c.ptoken.as_ref(), pool.as_ref()], &DAMM_ID),
            token_a_mint: c.base_mint,
            token_b_mint: c.ptoken,
            payer: *user,
            token_a_program: spl_token::ID,
            token_b_program: spl_token::ID,
            referral_token_account: None,
            event_authority: DAMM_EVENT_AUTHORITY,
            program: DAMM_ID,
        };
        let params = cp_amm::types::SwapParameters2 { amount_0: amount_in, amount_1: 0, swap_mode: 0 };
        Instruction {
            program_id: DAMM_ID,
            accounts: accounts.to_account_metas(None),
            data: cp_amm::client::args::Swap2 { _params: params }.data(),
        }
    }

    /* ------------------------------------------------------- graduation */

    /// DBC `migration_damm_v2` called directly (as anyone may), into the pool
    /// under `damm_config` (remaining account 0). Returns the instruction and
    /// the two position NFT mints.
    pub fn migrate_ix_into(&self, payer: &Pubkey, c: &Coin, damm_config: &Pubkey) -> (Instruction, Keypair, Keypair) {
        let (m1, m2) = (Keypair::new(), Keypair::new());
        let pool = damm_pool_address(damm_config, &c.base_mint, &c.ptoken);
        let accounts = dbc::client::accounts::MigrationDammV2 {
            virtual_pool: c.pool,
            migration_metadata: system_program::ID,
            config: c.config,
            pool_authority: DBC_POOL_AUTHORITY,
            pool,
            first_position_nft_mint: m1.pubkey(),
            first_position_nft_account: pda(&[b"position_nft_account", m1.pubkey().as_ref()], &DAMM_ID),
            first_position: pda(&[b"position", m1.pubkey().as_ref()], &DAMM_ID),
            second_position_nft_mint: Some(m2.pubkey()),
            second_position_nft_account: Some(pda(&[b"position_nft_account", m2.pubkey().as_ref()], &DAMM_ID)),
            second_position: Some(pda(&[b"position", m2.pubkey().as_ref()], &DAMM_ID)),
            damm_pool_authority: DAMM_POOL_AUTHORITY,
            amm_program: DAMM_ID,
            base_mint: c.base_mint,
            quote_mint: c.ptoken,
            token_a_vault: pda(&[b"token_vault", c.base_mint.as_ref(), pool.as_ref()], &DAMM_ID),
            token_b_vault: pda(&[b"token_vault", c.ptoken.as_ref(), pool.as_ref()], &DAMM_ID),
            base_vault: c.base_vault,
            quote_vault: c.quote_vault,
            payer: *payer,
            token_base_program: spl_token::ID,
            token_quote_program: spl_token::ID,
            token_2022_program: TOKEN_2022,
            damm_event_authority: DAMM_EVENT_AUTHORITY,
            system_program: system_program::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.push(AccountMeta::new_readonly(*damm_config, false));
        let ix = Instruction { program_id: DBC_ID, accounts: metas, data: dbc::client::args::MigrationDammV2 {}.data() };
        (ix, m1, m2)
    }

    /// DBC's migration called directly into the pinned `Hv8L…` pool.
    pub fn migrate_ix(&self, payer: &Pubkey, c: &Coin) -> (Instruction, Keypair, Keypair) {
        self.migrate_ix_into(payer, c, &DAMM_CONFIG)
    }

    /// `graduate` for the fee vault's position of NFT `nft`. With `second`,
    /// it also migrates (both NFT mints then sign); without, it records a
    /// migration someone else ran.
    pub fn graduate_ix(&self, payer: &Pubkey, c: &Coin, nft: &Pubkey, second: Option<&Pubkey>) -> Instruction {
        let pool = c.damm_pool();
        let accounts = pl::accounts::Graduate {
            payer: *payer,
            factory: self.factory(),
            launch: c.launch,
            dbc_pool: c.pool,
            dbc_config: c.config,
            dbc_pool_authority: DBC_POOL_AUTHORITY,
            dbc_base_vault: c.base_vault,
            dbc_quote_vault: c.quote_vault,
            base_mint: c.base_mint,
            ptoken_mint: c.ptoken,
            damm_config: DAMM_CONFIG,
            damm_pool: pool,
            damm_token_a_vault: pda(&[b"token_vault", c.base_mint.as_ref(), pool.as_ref()], &DAMM_ID),
            damm_token_b_vault: pda(&[b"token_vault", c.ptoken.as_ref(), pool.as_ref()], &DAMM_ID),
            position: pda(&[b"position", nft.as_ref()], &DAMM_ID),
            position_nft_account: pda(&[b"position_nft_account", nft.as_ref()], &DAMM_ID),
            position_nft_mint: second.map(|_| *nft),
            second_position_nft_mint: second.copied(),
            second_position_nft_account: second.map(|m| pda(&[b"position_nft_account", m.as_ref()], &DAMM_ID)),
            second_position: second.map(|m| pda(&[b"position", m.as_ref()], &DAMM_ID)),
            damm_pool_authority: DAMM_POOL_AUTHORITY,
            damm_event_authority: DAMM_EVENT_AUTHORITY,
            damm_program: DAMM_ID,
            dbc_program: DBC_ID,
            token_program: spl_token::ID,
            token_2022_program: TOKEN_2022,
            system_program: system_program::ID,
            event_authority: self.events(),
            program: pl::ID,
        };
        Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: pl::instruction::Graduate {}.data() }
    }

    /// The keeper's graduation: one `graduate` that runs DBC's migration
    /// through `Hv8L…` and records the pool, one v0 transaction.
    pub fn migrate(&mut self, c: &Coin) -> Res<(Sent, Pubkey)> {
        let k = self.keeper.insecure_clone();
        let (m1, m2) = (Keypair::new(), Keypair::new());
        let g = self.graduate_ix(&k.pubkey(), c, &m1.pubkey(), Some(&m2.pubkey()));
        let sent = self.send_v0(&[g], &k, &[&m1, &m2])?;
        Ok((sent, m1.pubkey()))
    }

    pub fn expire_ix(&self, c: &Coin, creator: &Pubkey) -> Instruction {
        let accounts = pl::accounts::ExpireLaunch {
            launch: c.launch,
            creator: *creator,
            dbc_pool: c.pool,
            event_authority: self.events(),
            program: pl::ID,
        };
        Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: pl::instruction::ExpireLaunch {}.data() }
    }

    /// The keeper's `sweep_fees` on DBC pool `pool` (config `config`, base
    /// mint `base`, quote mint `quote`).
    pub fn sweep_ix(&self, keeper: &Pubkey, config: &Pubkey, pool: &Pubkey, base: &Pubkey, quote: &Pubkey) -> Instruction {
        let platform = self.factory_state().platform;
        let accounts = pl::accounts::SweepFees {
            keeper: *keeper,
            factory: self.factory(),
            fee_vault: self.fee_vault(),
            launch: pda(&[LAUNCH_SEED, base.as_ref()], &pl::ID),
            quote_launch: pda(&[LAUNCH_SEED, quote.as_ref()], &pl::ID),
            base_mint: *base,
            quote_mint: *quote,
            vault_base: get_associated_token_address(&self.fee_vault(), base),
            vault_quote: get_associated_token_address(&self.fee_vault(), quote),
            base_owed: pda(&[PLATFORM_OWED_SEED, base.as_ref(), platform.as_ref()], &pl::ID),
            quote_owed: pda(&[PLATFORM_OWED_SEED, quote.as_ref(), platform.as_ref()], &pl::ID),
            dbc_pool_authority: DBC_POOL_AUTHORITY,
            dbc_config: *config,
            dbc_pool: *pool,
            dbc_base_vault: pda(&[b"token_vault", base.as_ref(), pool.as_ref()], &DBC_ID),
            dbc_quote_vault: pda(&[b"token_vault", quote.as_ref(), pool.as_ref()], &DBC_ID),
            dbc_event_authority: DBC_EVENT_AUTHORITY,
            dbc_program: DBC_ID,
            token_program: spl_token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: system_program::ID,
            event_authority: self.events(),
            program: pl::ID,
        };
        Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: pl::instruction::SweepFees {}.data() }
    }

    /// The keeper's `sweep_position_fees` on the fee vault's position of
    /// NFT `nft` in DAMM v2 pool `pool` (token A `a`, token B `b`).
    pub fn sweep_position_ix(&self, keeper: &Pubkey, pool: &Pubkey, a: &Pubkey, b: &Pubkey, nft: &Pubkey) -> Instruction {
        let platform = self.factory_state().platform;
        let accounts = pl::accounts::SweepPositionFees {
            keeper: *keeper,
            factory: self.factory(),
            fee_vault: self.fee_vault(),
            launch_a: pda(&[LAUNCH_SEED, a.as_ref()], &pl::ID),
            launch_b: pda(&[LAUNCH_SEED, b.as_ref()], &pl::ID),
            token_a_mint: *a,
            token_b_mint: *b,
            vault_a: get_associated_token_address(&self.fee_vault(), a),
            vault_b: get_associated_token_address(&self.fee_vault(), b),
            owed_a: pda(&[PLATFORM_OWED_SEED, a.as_ref(), platform.as_ref()], &pl::ID),
            owed_b: pda(&[PLATFORM_OWED_SEED, b.as_ref(), platform.as_ref()], &pl::ID),
            damm_pool_authority: DAMM_POOL_AUTHORITY,
            damm_pool: *pool,
            position: pda(&[b"position", nft.as_ref()], &DAMM_ID),
            position_nft_account: pda(&[b"position_nft_account", nft.as_ref()], &DAMM_ID),
            damm_token_a_vault: pda(&[b"token_vault", a.as_ref(), pool.as_ref()], &DAMM_ID),
            damm_token_b_vault: pda(&[b"token_vault", b.as_ref(), pool.as_ref()], &DAMM_ID),
            damm_event_authority: DAMM_EVENT_AUTHORITY,
            damm_program: DAMM_ID,
            token_program: spl_token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: system_program::ID,
            event_authority: self.events(),
            program: pl::ID,
        };
        Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: pl::instruction::SweepPositionFees {}.data() }
    }

    /// A position on the coin's graduated pool that `owner` funds with
    /// exactly `liquidity` and locks for good, as DBC locks the vault's;
    /// with `to_vault`, its NFT account is then handed to the fee vault
    /// (Token-2022 SetAuthority, as DBC hands over its own). Returns the
    /// NFT mint.
    pub fn locked_position(&mut self, owner: &Keypair, c: &Coin, liquidity: u128, to_vault: bool) -> Res<Pubkey> {
        let o = owner.pubkey();
        self.set_token(get_associated_token_address(&o, &c.base_mint), c.base_mint, o, u64::MAX / 4);
        self.set_token(get_associated_token_address(&o, &c.ptoken), c.ptoken, o, u64::MAX / 4);
        let (open, nft) = self.open_position_ix(&o, &o, c);
        let mut ixs = vec![
            open,
            self.add_liquidity_ix(&o, c, &nft.pubkey(), liquidity),
            self.lock_position_ix(&o, c, &nft.pubkey(), liquidity),
        ];
        if to_vault {
            let account = pda(&[b"position_nft_account", nft.pubkey().as_ref()], &DAMM_ID);
            let mut give = spl_token::instruction::set_authority(
                &spl_token::ID,
                &account,
                Some(&self.fee_vault()),
                spl_token::instruction::AuthorityType::AccountOwner,
                &o,
                &[],
            )
            .unwrap();
            give.program_id = TOKEN_2022;
            ixs.push(give);
        }
        self.send_v0(&ixs, owner, &[&nft])?;
        Ok(nft.pubkey())
    }

    /// The owner of a position NFT's Token-2022 account.
    pub fn nft_owner(&self, nft: &Pubkey) -> Pubkey {
        let a = self.svm.get_account(&pda(&[b"position_nft_account", nft.as_ref()], &DAMM_ID)).unwrap();
        Pubkey::new_from_array(a.data[32..64].try_into().unwrap())
    }

    /// cp-amm `create_position` on the coin's graduated pool for `owner`
    /// (anyone may open one, for any owner). Returns the NFT mint keypair.
    pub fn open_position_ix(&self, payer: &Pubkey, owner: &Pubkey, c: &Coin) -> (Instruction, Keypair) {
        let m = Keypair::new();
        let accounts = cp_amm::client::accounts::CreatePosition {
            owner: *owner,
            position_nft_mint: m.pubkey(),
            position_nft_account: pda(&[b"position_nft_account", m.pubkey().as_ref()], &DAMM_ID),
            pool: c.damm_pool(),
            position: pda(&[b"position", m.pubkey().as_ref()], &DAMM_ID),
            pool_authority: DAMM_POOL_AUTHORITY,
            payer: *payer,
            token_program: TOKEN_2022,
            system_program: system_program::ID,
            event_authority: DAMM_EVENT_AUTHORITY,
            program: DAMM_ID,
        };
        let ix = Instruction { program_id: DAMM_ID, accounts: accounts.to_account_metas(None), data: cp_amm::client::args::CreatePosition {}.data() };
        (ix, m)
    }

    /// cp-amm `add_liquidity` from `owner`'s coin and pToken accounts into
    /// the position of `nft_mint`.
    pub fn add_liquidity_ix(&self, owner: &Pubkey, c: &Coin, nft_mint: &Pubkey, liquidity: u128) -> Instruction {
        let pool = c.damm_pool();
        let accounts = cp_amm::client::accounts::AddLiquidity {
            pool,
            position: pda(&[b"position", nft_mint.as_ref()], &DAMM_ID),
            token_a_account: get_associated_token_address(owner, &c.base_mint),
            token_b_account: get_associated_token_address(owner, &c.ptoken),
            token_a_vault: pda(&[b"token_vault", c.base_mint.as_ref(), pool.as_ref()], &DAMM_ID),
            token_b_vault: pda(&[b"token_vault", c.ptoken.as_ref(), pool.as_ref()], &DAMM_ID),
            token_a_mint: c.base_mint,
            token_b_mint: c.ptoken,
            position_nft_account: pda(&[b"position_nft_account", nft_mint.as_ref()], &DAMM_ID),
            signer: *owner,
            token_a_program: spl_token::ID,
            token_b_program: spl_token::ID,
            event_authority: DAMM_EVENT_AUTHORITY,
            program: DAMM_ID,
        };
        let params = cp_amm::types::AddLiquidityParameters {
            liquidity_delta: liquidity,
            token_a_amount_threshold: u64::MAX,
            token_b_amount_threshold: u64::MAX,
        };
        Instruction { program_id: DAMM_ID, accounts: accounts.to_account_metas(None), data: cp_amm::client::args::AddLiquidity { params }.data() }
    }

    /// cp-amm `permanent_lock_position`, signed by the position's owner.
    pub fn lock_position_ix(&self, owner: &Pubkey, c: &Coin, nft_mint: &Pubkey, liquidity: u128) -> Instruction {
        let accounts = cp_amm::client::accounts::PermanentLockPosition {
            pool: c.damm_pool(),
            position: pda(&[b"position", nft_mint.as_ref()], &DAMM_ID),
            position_nft_account: pda(&[b"position_nft_account", nft_mint.as_ref()], &DAMM_ID),
            signer: *owner,
            event_authority: DAMM_EVENT_AUTHORITY,
            program: DAMM_ID,
        };
        Instruction {
            program_id: DAMM_ID,
            accounts: accounts.to_account_metas(None),
            data: cp_amm::client::args::PermanentLockPosition { permanent_lock_liquidity: liquidity }.data(),
        }
    }

    /* ------------------------------------------------------------- fees */

    pub fn collect_ix(&self, caller: &Pubkey, c: &Coin) -> Instruction {
        let l = self.launch_of(c);
        let g = l.graduated;
        let some = |k: Pubkey| if g { Some(k) } else { None };
        let pool = c.damm_pool();
        let accounts = pl::accounts::Collect {
            caller: *caller,
            factory: self.factory(),
            launch: c.launch,
            fee_vault: self.fee_vault(),
            ptoken_mint: c.ptoken,
            base_mint: c.base_mint,
            vault_ptoken: get_associated_token_address(&self.fee_vault(), &c.ptoken),
            vault_coin: get_associated_token_address(&self.fee_vault(), &c.base_mint),
            payee_owed: pda(&[OWED_SEED, c.ptoken.as_ref(), l.payee.as_ref()], &pl::ID),
            platform_owed: pda(&[PLATFORM_OWED_SEED, c.ptoken.as_ref(), self.factory_state().platform.as_ref()], &pl::ID),
            dbc_pool_authority: DBC_POOL_AUTHORITY,
            dbc_config: l.dbc_config,
            dbc_pool: l.pool,
            dbc_base_vault: c.base_vault,
            dbc_quote_vault: c.quote_vault,
            dbc_event_authority: DBC_EVENT_AUTHORITY,
            dbc_program: DBC_ID,
            damm_pool_authority: some(DAMM_POOL_AUTHORITY),
            damm_pool: some(pool),
            position: some(l.position),
            damm_token_a_vault: some(pda(&[b"token_vault", c.base_mint.as_ref(), pool.as_ref()], &DAMM_ID)),
            damm_token_b_vault: some(pda(&[b"token_vault", c.ptoken.as_ref(), pool.as_ref()], &DAMM_ID)),
            position_nft_account: some(l.position_nft_account),
            damm_event_authority: some(DAMM_EVENT_AUTHORITY),
            damm_program: some(DAMM_ID),
            token_program: spl_token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: system_program::ID,
            event_authority: self.events(),
            program: pl::ID,
        };
        Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: pl::instruction::Collect {}.data() }
    }

    pub fn collect(&mut self, c: &Coin) -> Res {
        let k = self.keeper.insecure_clone();
        let ix = self.collect_ix(&k.pubkey(), c);
        self.send_v0(&[ix], &k, &[])
    }

    /// `withdraw` of what `payee` is owed in `mint`, as a coin's payee or
    /// (`platform`) as the platform.
    pub fn withdraw_as(&mut self, payee: &Keypair, mint: &Pubkey, to: &Pubkey, platform: bool) -> Res {
        let seed = if platform { PLATFORM_OWED_SEED } else { OWED_SEED };
        let accounts = pl::accounts::Withdraw {
            payee: payee.pubkey(),
            factory: self.factory(),
            fee_vault: self.fee_vault(),
            owed: pda(&[seed, mint.as_ref(), payee.pubkey().as_ref()], &pl::ID),
            mint: *mint,
            vault: get_associated_token_address(&self.fee_vault(), mint),
            to: *to,
            token_program: spl_token::ID,
            event_authority: self.events(),
            program: pl::ID,
        };
        let ix = Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: pl::instruction::Withdraw {}.data() };
        self.send(&[ix], payee, &[])
    }

    pub fn withdraw(&mut self, payee: &Keypair, mint: &Pubkey, to: &Pubkey) -> Res {
        self.withdraw_as(payee, mint, to, false)
    }

    pub fn withdraw_holders(&mut self, who: &Keypair, c: &Coin, amount: u64, to: &Pubkey) -> Res {
        let accounts = pl::accounts::WithdrawHolders {
            keeper: who.pubkey(),
            factory: self.factory(),
            launch: c.launch,
            fee_vault: self.fee_vault(),
            mint: c.ptoken,
            vault: get_associated_token_address(&self.fee_vault(), &c.ptoken),
            to: *to,
            token_program: spl_token::ID,
            event_authority: self.events(),
            program: pl::ID,
        };
        let ix = Instruction {
            program_id: pl::ID,
            accounts: accounts.to_account_metas(None),
            data: pl::instruction::WithdrawHolders { amount }.data(),
        };
        self.send(&[ix], who, &[])
    }

    pub fn payee_ix<T: InstructionData>(&mut self, who: &Keypair, c: &Coin, data: T) -> Res {
        let accounts = pl::accounts::PayeeOnly {
            signer: who.pubkey(),
            factory: self.factory(),
            launch: c.launch,
            event_authority: self.events(),
            program: pl::ID,
        };
        self.send(&[Instruction { program_id: pl::ID, accounts: accounts.to_account_metas(None), data: data.data() }], who, &[])
    }
}
