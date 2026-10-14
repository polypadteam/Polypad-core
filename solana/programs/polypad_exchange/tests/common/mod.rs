//! LiteSVM harness for `polypad_exchange`: loads the built program
//! (`anchor build` first), a classic SPL Token USDC mint the tests control,
//! and builds every instruction from the program's own Anchor client types,
//! with a real ed25519 precompile instruction in front of each quoted one.

#![allow(dead_code)]

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program_option::COption;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::{system_program, AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use ed25519_dalek::{Signer as _, SigningKey};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_clock::Clock;
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

pub use polypad_exchange as px;
pub use px::error::ExchangeError;
pub use px::quote::{Quote, BUY, SELL};
pub use px::state::*;

/// 2027-01-15T08:00:00Z, on an hour boundary.
pub const T0: i64 = 1_800_000_000;
pub const USDC: u64 = 1_000_000;

pub fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &px::ID).0
}

pub fn pid(n: u8) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[31] = n;
    id[0] = 0x7f;
    id
}

pub struct Mkt {
    pub id: [u8; 32],
    pub market: Pubkey,
    pub mint: Pubkey,
    pub held: Pubkey,
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

pub struct Env {
    pub svm: LiteSVM,
    pub owner: Keypair,
    pub keeper: Keypair,
    pub signer: SigningKey,
    pub usdc_auth: Keypair,
    pub usdc_mint: Pubkey,
    pub config: Pubkey,
    pub vault: Pubkey,
    pub bridge: Pubkey,
    pub event_authority: Pubkey,
    pub now: i64,
    pub nonce: std::cell::Cell<u64>,
    /// Queue triples the next instruction built passes instead of the
    /// current ones: a list the client read earlier, or none.
    pub queue_list: std::cell::RefCell<Option<Vec<AccountMeta>>>,
}

pub type Res = Result<(), TransactionError>;

pub fn code(e: ExchangeError) -> u32 {
    e.into()
}

#[track_caller]
pub fn expect_err(r: Res, e: ExchangeError) {
    match r {
        Err(TransactionError::InstructionError(_, InstructionError::Custom(c))) if c == code(e) => {}
        other => panic!("expected {e:?} ({}), got {other:?}", code(e)),
    }
}

/// Any custom program error (Anchor constraint codes included).
#[track_caller]
pub fn expect_custom(r: Res, want: u32) {
    match r {
        Err(TransactionError::InstructionError(_, InstructionError::Custom(c))) if c == want => {}
        other => panic!("expected custom {want}, got {other:?}"),
    }
}

impl Env {
    pub fn new() -> Env {
        let mut env = Env::bare();
        env.initialize(&env.owner.insecure_clone()).unwrap();
        // The first bridge deposit is set at once.
        let (keeper, bridge) = (env.keeper.pubkey(), env.bridge);
        env.owner_ix(px::instruction::SetRoles { keeper, factory: Pubkey::default(), bridge_deposit: bridge }).unwrap();
        env
    }

    /// Program loaded, nothing initialized.
    pub fn bare() -> Env {
        let mut svm = LiteSVM::new();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/polypad_exchange.so");
        let so = std::fs::read(path).expect("run `anchor build` first: target/deploy/polypad_exchange.so is missing");
        svm.add_program(px::ID, &so).unwrap();
        let owner = Keypair::new();
        // LiteSVM deploys with no upgrade authority; make the owner the
        // authority, as a real deploy would, so it may initialize.
        let pd = Pubkey::find_program_address(&[px::ID.as_ref()], &anchor_lang::solana_program::bpf_loader_upgradeable::ID).0;
        let mut acc = svm.get_account(&pd).unwrap();
        acc.data[12] = 1;
        acc.data[13..45].copy_from_slice(owner.pubkey().as_ref());
        svm.set_account(pd, acc).unwrap();

        let keeper = Keypair::new();
        let usdc_auth = Keypair::new();
        for k in [&owner, &keeper, &usdc_auth] {
            svm.airdrop(&k.pubkey(), 100_000_000_000).unwrap();
        }
        let usdc_mint = Pubkey::new_unique();
        let mut env = Env {
            svm,
            owner,
            keeper,
            signer: SigningKey::from_bytes(&[42; 32]),
            usdc_auth,
            usdc_mint,
            config: pda(&[CONFIG_SEED]),
            vault: pda(&[VAULT_SEED]),
            bridge: Pubkey::new_unique(),
            event_authority: pda(&[b"__event_authority"]),
            now: T0,
            nonce: std::cell::Cell::new(0),
            queue_list: std::cell::RefCell::new(None),
        };
        env.set_mint(usdc_mint, 6, Some(env.usdc_auth.pubkey()), Some(env.usdc_auth.pubkey()));
        let bridge_owner = Pubkey::new_unique();
        env.set_token(env.bridge, usdc_mint, bridge_owner, 0);
        env.warp_to(T0);
        env
    }

    pub fn initialize(&mut self, payer: &Keypair) -> Res {
        let signer = self.signer_key();
        let accounts = px::accounts::Initialize {
            payer: payer.pubkey(),
            config: self.config,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            exchange_program: px::ID,
            program_data: Pubkey::find_program_address(&[px::ID.as_ref()], &anchor_lang::solana_program::bpf_loader_upgradeable::ID).0,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let data = px::instruction::Initialize { owner: self.owner.pubkey(), keeper: self.keeper.pubkey(), signer }.data();
        let ix = Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data };
        self.send(&[ix], payer, &[])
    }

    pub fn signer_key(&self) -> Pubkey {
        Pubkey::new_from_array(self.signer.verifying_key().to_bytes())
    }

    /* ------------------------------------------------------------- chain */

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

    pub fn send(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Res {
        self.send_meta(ixs, payer, extra).map(|_| ())
    }

    /// Sends and returns compute units used.
    pub fn send_meta(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> Result<u64, TransactionError> {
        let mut signers: Vec<&Keypair> = vec![payer];
        for k in extra {
            if k.pubkey() != payer.pubkey() {
                signers.push(k);
            }
        }
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, self.svm.latest_blockhash());
        let r = self.svm.send_transaction(tx);
        self.svm.expire_blockhash();
        match r {
            Ok(m) => Ok(m.compute_units_consumed),
            Err(e) => {
                if std::env::var("PX_LOGS").is_ok() {
                    eprintln!("{}", e.meta.logs.join("\n"));
                    for ix in ixs {
                        for a in &ix.accounts {
                            let acc = self.svm.get_account(&a.pubkey);
                            eprintln!("  {} w={} s={} lamports={:?} owner={:?} len={:?}", a.pubkey, a.is_writable, a.is_signer, acc.as_ref().map(|x| x.lamports), acc.as_ref().map(|x| x.owner), acc.as_ref().map(|x| x.data.len()));
                        }
                    }
                }
                Err(e.err)
            }
        }
    }

    pub fn owner_ix<T: InstructionData>(&mut self, data: T) -> Res {
        let ix = self.admin_ix(self.owner.pubkey(), data);
        let owner = self.owner.insecure_clone();
        self.send(&[ix], &owner, &[])
    }

    pub fn admin_ix<T: InstructionData>(&self, who: Pubkey, data: T) -> Instruction {
        let accounts = px::accounts::OwnerOnly {
            owner: who,
            config: self.config,
            event_authority: self.event_authority,
            program: px::ID,
        };
        Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: data.data() }
    }

    pub fn as_kp(k: &Keypair) -> Keypair {
        k.insecure_clone()
    }

    /* ------------------------------------------------------------ tokens */

    pub fn set_mint(&mut self, addr: Pubkey, decimals: u8, authority: Option<Pubkey>, freeze: Option<Pubkey>) {
        let m = spl_token::state::Mint {
            mint_authority: authority.map_or(COption::None, COption::Some),
            supply: 0,
            decimals,
            is_initialized: true,
            freeze_authority: freeze.map_or(COption::None, COption::Some),
        };
        let mut data = vec![0u8; spl_token::state::Mint::LEN];
        m.pack_into_slice(&mut data);
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm
            .set_account(addr, Account { lamports, data, owner: spl_token::ID, executable: false, rent_epoch: 0 })
            .unwrap();
    }

    /// A token account set directly (balances for tests; supply is not tracked for USDC).
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
        self.svm
            .set_account(addr, Account { lamports, data, owner: spl_token::ID, executable: false, rent_epoch: 0 })
            .unwrap();
    }

    pub fn token(&self, addr: &Pubkey) -> spl_token::state::Account {
        spl_token::state::Account::unpack(&self.svm.get_account(addr).unwrap().data).unwrap()
    }

    pub fn balance(&self, addr: &Pubkey) -> u64 {
        self.token(addr).amount
    }

    pub fn supply(&self, mint: &Pubkey) -> u64 {
        spl_token::state::Mint::unpack(&self.svm.get_account(mint).unwrap().data).unwrap().supply
    }

    /// USDC arriving in the float from outside (the bridge, a donation).
    pub fn fund_vault(&mut self, amount: u64) {
        let ix = spl_token::instruction::mint_to(&spl_token::ID, &self.usdc_mint, &self.vault, &self.usdc_auth.pubkey(), &[], amount).unwrap();
        let a = Env::as_kp(&self.usdc_auth);
        self.send(&[ix], &a, &[]).unwrap();
    }

    pub fn freeze(&mut self, account: &Pubkey, frozen: bool) {
        let ix = if frozen {
            spl_token::instruction::freeze_account(&spl_token::ID, account, &self.usdc_mint, &self.usdc_auth.pubkey(), &[]).unwrap()
        } else {
            spl_token::instruction::thaw_account(&spl_token::ID, account, &self.usdc_mint, &self.usdc_auth.pubkey(), &[]).unwrap()
        };
        let a = Env::as_kp(&self.usdc_auth);
        self.send(&[ix], &a, &[]).unwrap();
    }

    pub fn user(&mut self, usdc: u64) -> User {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
        let acct = Pubkey::new_unique();
        let (mint, owner) = (self.usdc_mint, kp.pubkey());
        self.set_token(acct, mint, owner, 0);
        if usdc > 0 {
            let ix = spl_token::instruction::mint_to(&spl_token::ID, &self.usdc_mint, &acct, &self.usdc_auth.pubkey(), &[], usdc).unwrap();
            let a = Env::as_kp(&self.usdc_auth);
            self.send(&[ix], &a, &[]).unwrap();
        }
        User { kp, usdc: acct }
    }

    /// The user's pToken account for a market (created empty if missing).
    pub fn ptoken_account(&mut self, u: &User, m: &Mkt) -> Pubkey {
        let addr = Pubkey::find_program_address(&[u.key().as_ref(), m.mint.as_ref()], &px::ID).0;
        if self.svm.get_account(&addr).is_none() {
            let (mint, owner) = (m.mint, u.key());
            self.set_token(addr, mint, owner, 0);
        }
        addr
    }

    /* ------------------------------------------------------------- state */

    pub fn config(&self) -> Config {
        Config::try_deserialize(&mut &self.svm.get_account(&self.config).unwrap().data[..]).unwrap()
    }

    pub fn market(&self, m: &Mkt) -> Market {
        Market::try_deserialize(&mut &self.svm.get_account(&m.market).unwrap().data[..]).unwrap()
    }

    pub fn claim_at(&self, addr: &Pubkey) -> Option<Claim> {
        let a = self.svm.get_account(addr)?;
        if a.data.is_empty() {
            return None;
        }
        Some(Claim::try_deserialize(&mut &a.data[..]).unwrap())
    }

    pub fn ticket_at(&self, n: u64) -> Option<DelayedSale> {
        let a = self.svm.get_account(&pda(&[DELAYED_SEED, &n.to_le_bytes()]))?;
        if a.data.is_empty() {
            return None;
        }
        Some(DelayedSale::try_deserialize(&mut &a.data[..]).unwrap())
    }

    pub fn exists(&self, addr: &Pubkey) -> bool {
        self.svm.get_account(addr).is_some_and(|a| a.lamports > 0)
    }

    /* ----------------------------------------------------------- markets */

    pub fn market_keys(id: [u8; 32]) -> Mkt {
        Mkt {
            id,
            market: pda(&[MARKET_SEED, &id]),
            mint: pda(&[PTOKEN_SEED, &id]),
            held: pda(&[HELD_SEED, &id]),
        }
    }

    pub fn create_market_by(&mut self, who: &Keypair, id: [u8; 32]) -> Result<Mkt, TransactionError> {
        let m = Env::market_keys(id);
        let accounts = px::accounts::CreateMarket {
            payer: who.pubkey(),
            authority: who.pubkey(),
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            held: m.held,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let ix = Instruction {
            program_id: px::ID,
            accounts: accounts.to_account_metas(None),
            data: px::instruction::CreateMarket { position_id: id }.data(),
        };
        self.send(&[ix], who, &[])?;
        Ok(m)
    }

    pub fn create_market(&mut self, id: [u8; 32]) -> Mkt {
        let o = Env::as_kp(&self.owner);
        self.create_market_by(&o, id).unwrap()
    }

    pub fn market_admin(&mut self, who: &Keypair, m: &Mkt, data: impl InstructionData) -> Res {
        let accounts = px::accounts::MarketAdmin {
            authority: who.pubkey(),
            config: self.config,
            market: m.market,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let ix = Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: data.data() };
        self.send(&[ix], who, &[])
    }

    pub fn settle(&mut self, who: &Keypair, m: &Mkt, payout: u64) -> Res {
        self.market_admin(who, m, px::instruction::Settle { payout })
    }

    pub fn cancel_settle(&mut self, who: &Keypair, m: &Mkt) -> Res {
        self.market_admin(who, m, px::instruction::CancelSettle {})
    }

    pub fn set_paused(&mut self, who: &Keypair, ms: &[&Mkt], paused: bool) -> Res {
        let accounts = px::accounts::KeeperBatch {
            keeper: who.pubkey(),
            config: self.config,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(ms.iter().map(|m| AccountMeta::new(m.market, false)));
        let ix = Instruction { program_id: px::ID, accounts: metas, data: px::instruction::SetPaused { paused }.data() };
        self.send(&[ix], who, &[])
    }

    /// The desk's holdings as it read them, the market then at `seen` burned and `minted` minted.
    pub fn report(&mut self, m: &Mkt, amount: u64, seen: u64, minted: u64) -> Res {
        self.report_batch(&[(m, amount, seen, minted)])
    }

    /// One `report_backed` of `(market, amount, seen, minted)` entries, in
    /// order; a market may appear more than once.
    pub fn report_batch(&mut self, entries: &[(&Mkt, u64, u64, u64)]) -> Res {
        let k = Env::as_kp(&self.keeper);
        let accounts = px::accounts::KeeperBatch {
            keeper: k.pubkey(),
            config: self.config,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        for (m, ..) in entries {
            metas.push(AccountMeta::new(m.market, false));
            metas.push(AccountMeta::new_readonly(m.mint, false));
        }
        let ix = Instruction {
            program_id: px::ID,
            accounts: metas,
            data: px::instruction::ReportBacked {
                amounts: entries.iter().map(|e| e.1).collect(),
                seen: entries.iter().map(|e| e.2).collect(),
                minted: entries.iter().map(|e| e.3).collect(),
            }
            .data(),
        };
        self.send(&[ix], &k, &[])
    }

    /// Holdings read now: the market's counts from the view.
    pub fn report_now(&mut self, m: &Mkt, amount: u64) -> Res {
        let v = self.view_market(m);
        self.report(m, amount, v.burned_total, v.minted_total)
    }

    /* ------------------------------------------------------------ quotes */

    /// A quote as the pricer would sign it. Two identical requests in the same
    /// second would otherwise be the same quote with one shared fill, so each
    /// gets a few base units of extra `max_amount`, as distinct quotes do.
    pub fn quote(&self, m: &Mkt, side: u8, price: u64, max_amount: u64) -> Quote {
        let n = self.nonce.get();
        self.nonce.set(n + 1);
        Quote { position_id: m.id, side, price, max_amount: max_amount + n, valid_until: self.now + 15 }
    }

    /// The precompile instruction a client builds: header, pubkey, signature, message.
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
        Instruction { program_id: solana_sdk_ids::ed25519_program::ID, accounts: vec![], data: d }
    }

    pub fn ed25519_ix(&self, q: &Quote) -> Instruction {
        Env::ed25519_ix_by(&self.signer, &q.message(&px::ID))
    }

    /// `(claim, recipient, rent_to)` triples for the next `n` claims in line,
    /// or the list set in `queue_list` (used once).
    pub fn queue_metas(&self, n: usize) -> Vec<AccountMeta> {
        if let Some(v) = self.queue_list.borrow_mut().take() {
            return v;
        }
        let c = self.config();
        let mut v = vec![];
        for i in c.claim_head..c.claim_tail.min(c.claim_head + n as u64) {
            let addr = pda(&[CLAIM_SEED, &i.to_le_bytes()]);
            let cl = self.claim_at(&addr).expect("claim in line");
            v.push(AccountMeta::new(addr, false));
            v.push(AccountMeta::new(cl.to, false));
            v.push(AccountMeta::new(cl.payer, false));
        }
        v
    }

    pub fn next_claim(&self) -> Pubkey {
        pda(&[CLAIM_SEED, &self.config().claim_tail.to_le_bytes()])
    }

    pub fn next_aside(&self) -> Pubkey {
        pda(&[ASIDE_SEED, &self.config().aside_next.to_le_bytes()])
    }

    pub fn next_ticket(&self) -> Pubkey {
        pda(&[DELAYED_SEED, &self.config().delayed_next.to_le_bytes()])
    }

    /* ------------------------------------------------------------- trade */

    #[allow(clippy::too_many_arguments)]
    pub fn mint_ix(&self, u: &Pubkey, usdc: &Pubkey, to: &Pubkey, m: &Mkt, usdc_in: u64, min_out: u64, q: &Quote) -> Instruction {
        let accounts = px::accounts::MintQuoted {
            buyer: *u,
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            buyer_usdc: *usdc,
            to: *to,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            fill: pda(&[FILL_SEED, &q.fill_seed()]),
            instructions: solana_sdk_ids::sysvar::instructions::ID,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(self.queue_metas(QUEUE_BATCH));
        Instruction {
            program_id: px::ID,
            accounts: metas,
            data: px::instruction::Mint { usdc_in, min_out, quote: *q }.data(),
        }
    }

    /// Buy with a fresh quote at `price` signed by the oracle signer.
    pub fn buy(&mut self, u: &User, m: &Mkt, usdc_in: u64, price: u64) -> Res {
        let q = self.quote(m, BUY, price, usdc_in);
        self.buy_with(u, m, usdc_in, &q)
    }

    pub fn buy_with(&mut self, u: &User, m: &Mkt, usdc_in: u64, q: &Quote) -> Res {
        let to = self.ptoken_account(u, m);
        let ix = self.mint_ix(&u.key(), &u.usdc, &to, m, usdc_in, 0, q);
        let ed = self.ed25519_ix(q);
        let kp = Env::as_kp(&u.kp);
        self.send(&[ed, ix], &kp, &[])
    }

    pub fn mint_at_payout(&mut self, u: &User, m: &Mkt, usdc_in: u64) -> Res {
        let to = self.ptoken_account(u, m);
        let accounts = px::accounts::MintAtPayout {
            buyer: u.key(),
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            buyer_usdc: u.usdc,
            to,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            token_program: spl_token::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(self.queue_metas(QUEUE_BATCH));
        let ix = Instruction { program_id: px::ID, accounts: metas, data: px::instruction::MintAtPayout { usdc_in, min_out: 0 }.data() };
        let kp = Env::as_kp(&u.kp);
        self.send(&[ix], &kp, &[])
    }

    pub fn redeem_ix(&self, u: &User, m: &Mkt, amount_in: u64, min_out: u64, q: &Quote, to: &Pubkey) -> Instruction {
        let seller_ptoken = Pubkey::find_program_address(&[u.key().as_ref(), m.mint.as_ref()], &px::ID).0;
        let accounts = px::accounts::RedeemQuoted {
            seller: u.key(),
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            seller_ptoken,
            held: m.held,
            to: *to,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            fill: pda(&[FILL_SEED, &q.fill_seed()]),
            instructions: solana_sdk_ids::sysvar::instructions::ID,
            new_claim: Some(self.next_claim()),
            new_ticket: Some(self.next_ticket()),
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(self.queue_metas(QUEUE_BATCH));
        Instruction {
            program_id: px::ID,
            accounts: metas,
            data: px::instruction::Redeem { amount_in, min_out, quote: *q }.data(),
        }
    }

    pub fn sell(&mut self, u: &User, m: &Mkt, amount_in: u64, price: u64) -> Res {
        let q = self.quote(m, SELL, price, amount_in);
        self.sell_with(u, m, amount_in, &q)
    }

    pub fn sell_with(&mut self, u: &User, m: &Mkt, amount_in: u64, q: &Quote) -> Res {
        let to = u.usdc;
        let ix = self.redeem_ix(u, m, amount_in, 0, q, &to);
        let ed = self.ed25519_ix(q);
        let kp = Env::as_kp(&u.kp);
        self.send(&[ed, ix], &kp, &[])
    }

    pub fn redeem_at_payout(&mut self, u: &User, m: &Mkt, amount_in: u64) -> Res {
        let seller_ptoken = Pubkey::find_program_address(&[u.key().as_ref(), m.mint.as_ref()], &px::ID).0;
        let accounts = px::accounts::RedeemAtPayout {
            seller: u.key(),
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            seller_ptoken,
            held: m.held,
            to: u.usdc,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            new_claim: Some(self.next_claim()),
            new_ticket: Some(self.next_ticket()),
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(self.queue_metas(QUEUE_BATCH));
        let ix = Instruction { program_id: px::ID, accounts: metas, data: px::instruction::RedeemAtPayout { amount_in, min_out: 0 }.data() };
        let kp = Env::as_kp(&u.kp);
        self.send(&[ix], &kp, &[])
    }

    pub fn pay_queue(&mut self, max: u8) -> Res {
        let accounts = px::accounts::PayQueue {
            config: self.config,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            token_program: spl_token::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(self.queue_metas(max as usize));
        let ix = Instruction { program_id: px::ID, accounts: metas, data: px::instruction::PayQueue { max }.data() };
        let payer = Env::as_kp(&self.usdc_auth);
        self.send(&[ix], &payer, &[])
    }

    pub fn withdraw_unclaimed(&mut self, claim: Pubkey) -> Res {
        let cl = self.claim_at(&claim).unwrap();
        let accounts = px::accounts::WithdrawUnclaimed {
            config: self.config,
            claim,
            rent_to: cl.payer,
            to: cl.to,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            token_program: spl_token::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let ix = Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::WithdrawUnclaimed {}.data() };
        let payer = Env::as_kp(&self.usdc_auth);
        self.send(&[ix], &payer, &[])
    }

    pub fn release(&mut self, ticket: u64) -> Res {
        let t = self.ticket_at(ticket).expect("live ticket");
        let m = Env::market_keys(t.position_id);
        let caller = Env::as_kp(&self.usdc_auth);
        let accounts = px::accounts::Release {
            caller: caller.pubkey(),
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            held: m.held,
            ticket: pda(&[DELAYED_SEED, &ticket.to_le_bytes()]),
            rent_to: t.payer,
            to: t.to,
            vault: self.vault,
            usdc_mint: self.usdc_mint,
            new_claim: Some(self.next_claim()),
            new_aside: Some(self.next_aside()),
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(self.queue_metas(QUEUE_BATCH));
        let ix = Instruction { program_id: px::ID, accounts: metas, data: px::instruction::Release {}.data() };
        self.send(&[ix], &caller, &[])
    }

    pub fn cancel_by(&mut self, who: &Keypair, ticket: u64, dest: Pubkey) -> Res {
        let t = self.ticket_at(ticket).expect("live ticket");
        let m = Env::market_keys(t.position_id);
        let accounts = px::accounts::Cancel {
            owner: who.pubkey(),
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            held: m.held,
            ticket: pda(&[DELAYED_SEED, &ticket.to_le_bytes()]),
            rent_to: t.payer,
            dest,
            token_program: spl_token::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let ix = Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::Cancel {}.data() };
        self.send(&[ix], who, &[])
    }

    pub fn send_to_bridge_by(&mut self, who: &Keypair, to: Pubkey, amount: u64) -> Res {
        let accounts = px::accounts::SendToBridge {
            keeper: who.pubkey(),
            config: self.config,
            vault: self.vault,
            bridge: to,
            usdc_mint: self.usdc_mint,
            token_program: spl_token::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let ix = Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::SendToBridge { amount }.data() };
        self.send(&[ix], who, &[])
    }

    pub fn halt_by(&mut self, who: &Keypair) -> Res {
        let accounts = px::accounts::KeeperOrOwner {
            authority: who.pubkey(),
            config: self.config,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let ix = Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::Halt {}.data() };
        self.send(&[ix], who, &[])
    }

    pub fn absorb(&mut self, m: &Mkt) -> Res {
        self.absorb_with(m, None)
    }

    /// Absorb, also burning all of `other` (a pToken account the exchange PDA owns).
    pub fn absorb_with(&mut self, m: &Mkt, other: Option<Pubkey>) -> Res {
        let accounts = px::accounts::Absorb {
            config: self.config,
            market: m.market,
            ptoken_mint: m.mint,
            held: m.held,
            other,
            token_program: spl_token::ID,
            event_authority: self.event_authority,
            program: px::ID,
        };
        let ix = Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::Absorb {}.data() };
        let payer = Env::as_kp(&self.usdc_auth);
        self.send(&[ix], &payer, &[])
    }

    /// A holder burns their own pToken straight through SPL Token, which the
    /// exchange never sees (on the EVM only the exchange could burn).
    pub fn burn_direct(&mut self, u: &User, m: &Mkt, amount: u64) -> Res {
        let mine = self.ptoken_account(u, m);
        let ix = spl_token::instruction::burn(&spl_token::ID, &mine, &m.mint, &u.key(), &[], amount).unwrap();
        let kp = Env::as_kp(&u.kp);
        self.send(&[ix], &kp, &[])
    }

    /// A holder sends pToken to another token account.
    pub fn transfer_ptoken(&mut self, u: &User, m: &Mkt, to: Pubkey, amount: u64) -> Res {
        let mine = self.ptoken_account(u, m);
        let ix = spl_token::instruction::transfer(&spl_token::ID, &mine, &to, &u.key(), &[], amount).unwrap();
        let kp = Env::as_kp(&u.kp);
        self.send(&[ix], &kp, &[])
    }

    /// The config PDA's associated token account for a market's pToken,
    /// created empty if missing (as anyone could create it).
    pub fn exchange_ata(&mut self, m: &Mkt) -> Pubkey {
        let ata_program = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
        let addr = Pubkey::find_program_address(&[self.config.as_ref(), spl_token::ID.as_ref(), m.mint.as_ref()], &ata_program).0;
        if self.svm.get_account(&addr).is_none() {
            let (mint, owner) = (m.mint, self.config);
            self.set_token(addr, mint, owner, 0);
        }
        addr
    }

    pub fn refresh_risk(&mut self, ms: &[&Mkt]) -> Res {
        let accounts = px::accounts::AnyoneBatch { config: self.config };
        let mut metas = accounts.to_account_metas(None);
        for m in ms {
            metas.push(AccountMeta::new(m.market, false));
            metas.push(AccountMeta::new_readonly(m.mint, false));
        }
        let ix = Instruction { program_id: px::ID, accounts: metas, data: px::instruction::RefreshRisk {}.data() };
        let payer = Env::as_kp(&self.usdc_auth);
        self.send(&[ix], &payer, &[])
    }

    /// Simulate a read-only instruction and decode its return data.
    pub fn view<T: anchor_lang::AnchorDeserialize>(&self, ix: Instruction) -> Result<T, TransactionError> {
        let payer = Env::as_kp(&self.usdc_auth);
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], self.svm.latest_blockhash());
        let r = self.svm.simulate_transaction(tx).map_err(|e| e.err)?;
        assert_eq!(r.meta.return_data.program_id, px::ID);
        Ok(T::try_from_slice(&r.meta.return_data.data).unwrap())
    }

    pub fn view_float(&self) -> px::views::FloatView {
        let accounts = px::accounts::ViewFloat { config: self.config, vault: self.vault };
        self.view(Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::ViewFloat {}.data() }).unwrap()
    }

    pub fn view_market(&self, m: &Mkt) -> px::views::MarketView {
        let accounts = px::accounts::ViewMarket { config: self.config, market: m.market, ptoken_mint: m.mint };
        self.view(Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: px::instruction::ViewMarket {}.data() }).unwrap()
    }

    pub fn preview<T: InstructionData>(&self, data: T) -> u64 {
        let accounts = px::accounts::ViewConfig { config: self.config };
        self.view(Instruction { program_id: px::ID, accounts: accounts.to_account_metas(None), data: data.data() }).unwrap()
    }

    /// Outflow cap to exactly `floor` (no float share), metering sales.
    pub fn flat_cap(&mut self, floor: u64) {
        self.owner_ix(px::instruction::SetOutflowCap { floor, float_bps: 0 }).unwrap();
    }

    pub fn uncapped(&mut self) {
        self.owner_ix(px::instruction::SetMaxTotalRisk { cap: u64::MAX }).unwrap();
        self.owner_ix(px::instruction::SetParams {
            buy_spread_bps: 25,
            sell_spread_bps: 25,
            min_price: 1_000,
            max_price: 999_000,
            default_max_risk: u64::MAX,
        })
        .unwrap();
    }
}

/// Shares minted for `usdc_in` at `price` with the 0.25% spread, as the contract rounds.
pub fn shares_for(usdc_in: u64, price: u64) -> u64 {
    let eff = (price as u128 * 10_025).div_ceil(10_000);
    (usdc_in as u128 * 1_000_000 / eff) as u64
}

/// USDC paid for `amount` shares at `price` less `fee_bps`.
pub fn usdc_for(amount: u64, price: u64, fee_bps: u64) -> u64 {
    (amount as u128 * (price as u128 * (10_000 - fee_bps as u128) / 10_000) / 1_000_000) as u64
}
