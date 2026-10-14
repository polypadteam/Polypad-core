//! Signed quotes: the Solana form of `PriceOracle.verify`.
//!
//! The pricer signs a fixed-layout message with its ed25519 key. The taker's
//! transaction carries an `Ed25519SigVerify` instruction (the runtime's
//! precompile checks the signature) immediately before the instruction that
//! uses the quote. That instruction reads the precompile instruction back
//! through the Instructions sysvar and checks that it verified exactly:
//!
//! - one signature,
//! - by the configured signer,
//! - over the message this program rebuilds from the instruction's own
//!   arguments, byte for byte,
//! - with every offset pointing inside the precompile instruction's own data.
//!
//! The last point is where real systems failed: Wormhole (2022) trusted an
//! unchecked sysvar account, Relay read the key and message at fixed offsets
//! while the precompile verified whatever the offsets named. So every header
//! field is checked, and the sysvar is only read through the `_checked`
//! loaders. The sysvar lists top-level instructions only, so when `mint` or
//! `redeem` is reached through CPI the check reads the instruction before
//! the outer one. That is safe because a quote binds no taker: whoever holds
//! a signed quote may use it, through any program, and its fill cap holds
//! however it is reached. The launch program relies on this to mint inside
//! `launch`.
//!
//! ## Message (106 bytes)
//!
//! ```text
//! "POLYPAD_QUOTE_V1"  16  domain tag
//! program id          32  this program: a quote for another deployment never verifies here
//! cluster              1  0 devnet/localnet, 1 mainnet (feature `mainnet`)
//! position id         32  Polymarket CTF position id, big-endian (the EVM uint256)
//! side                 1  0 BUY, 1 SELL
//! price                8  u64 LE, 6 decimals ($1 = 1_000_000)
//! max amount           8  u64 LE, base units: USDC in for a BUY, pToken in for a SELL
//! valid until          8  i64 LE, unix seconds
//! ```
//!
//! The fill PDA for a quote is seeded by `sha256(message)` (never the
//! signature, which is malleable), as the EVM exchange keys `quoteFilled` by
//! the EIP-712 digest.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use solana_instructions_sysvar::{load_current_index_checked, load_instruction_at_checked};
use solana_sdk_ids::ed25519_program;

use crate::error::QuoteError;

pub const DOMAIN: &[u8; 16] = b"POLYPAD_QUOTE_V1";
pub const MESSAGE_LEN: usize = 16 + 32 + 1 + 32 + 1 + 8 + 8 + 8;

pub const BUY: u8 = 0;
pub const SELL: u8 = 1;

/// Longest a quote may be valid for, whatever the signer wrote (`PriceOracle.MAX_VALIDITY`).
pub const MAX_VALIDITY: i64 = 30;
/// Highest price any quote may carry (`PriceOracle.MAX_PRICE`): 99.9c.
pub const MAX_PRICE: u64 = 999_000;

#[cfg(feature = "mainnet")]
pub const CLUSTER: u8 = 1;
#[cfg(not(feature = "mainnet"))]
pub const CLUSTER: u8 = 0;

/// Ed25519 precompile header: num_signatures (u8), padding (u8), then one
/// 14-byte offsets record per signature.
const HEADER_LEN: usize = 16;
const SIGNATURE_LEN: usize = 64;
const PUBKEY_LEN: usize = 32;
/// "Read from this same instruction's data", in the precompile's index fields.
const THIS_INSTRUCTION: u16 = u16::MAX;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub position_id: [u8; 32],
    pub side: u8,
    pub price: u64,
    pub max_amount: u64,
    pub valid_until: i64,
}

impl Quote {
    /// The exact bytes the pricer signs for `program_id`.
    pub fn message(&self, program_id: &Pubkey) -> [u8; MESSAGE_LEN] {
        let mut m = [0u8; MESSAGE_LEN];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            m[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        put(DOMAIN);
        put(program_id.as_ref());
        put(&[CLUSTER]);
        put(&self.position_id);
        put(&[self.side]);
        put(&self.price.to_le_bytes());
        put(&self.max_amount.to_le_bytes());
        put(&self.valid_until.to_le_bytes());
        m
    }

    /// Seed of this quote's fill PDA: sha256 of the message, so the fill is
    /// tied to exactly what was signed (never to the malleable signature).
    pub fn fill_seed(&self) -> [u8; 32] {
        solana_sha256_hasher::hash(&self.message(&crate::ID)).to_bytes()
    }

    /// Checks on the quote's own fields (`PriceOracle._verify`, before the
    /// signature): the side wanted, live, not valid for longer than
    /// MAX_VALIDITY, and a price in (0, MAX_PRICE].
    pub fn check_fields(&self, side: u8, now: i64) -> std::result::Result<(), QuoteError> {
        if self.side != side {
            return Err(QuoteError::WrongSide);
        }
        if now > self.valid_until {
            return Err(QuoteError::QuoteExpired);
        }
        if self.valid_until > now.saturating_add(MAX_VALIDITY) {
            return Err(QuoteError::QuoteTooLong);
        }
        if self.price == 0 || self.price > MAX_PRICE {
            return Err(QuoteError::PriceOutOfRange);
        }
        Ok(())
    }
}

fn read_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

/// Whether `ix` is an ed25519 precompile instruction that verified exactly one
/// signature by `signer` over exactly `message`, with all of it read from its
/// own data. Pure: tested directly below.
pub fn check_ed25519_instruction(
    ix: &Instruction,
    signer: &Pubkey,
    message: &[u8],
) -> std::result::Result<(), QuoteError> {
    if ix.program_id != ed25519_program::ID {
        return Err(QuoteError::NotEd25519);
    }
    if !ix.accounts.is_empty() {
        return Err(QuoteError::BadEd25519Header);
    }
    let d = &ix.data;
    if d.len() < HEADER_LEN || d[0] != 1 {
        return Err(QuoteError::BadEd25519Header);
    }
    let sig_off = read_u16(d, 2) as usize;
    let sig_ix = read_u16(d, 4);
    let pk_off = read_u16(d, 6) as usize;
    let pk_ix = read_u16(d, 8);
    let msg_off = read_u16(d, 10) as usize;
    let msg_len = read_u16(d, 12) as usize;
    let msg_ix = read_u16(d, 14);

    // All three must point at this instruction's own data: an index naming
    // another instruction would let the precompile verify bytes this check
    // never looks at.
    if sig_ix != THIS_INSTRUCTION || pk_ix != THIS_INSTRUCTION || msg_ix != THIS_INSTRUCTION {
        return Err(QuoteError::BadEd25519Header);
    }
    // Every slice past the header and inside the data.
    let inside = |off: usize, len: usize| off >= HEADER_LEN && off.checked_add(len).is_some_and(|end| end <= d.len());
    if !inside(sig_off, SIGNATURE_LEN) || !inside(pk_off, PUBKEY_LEN) || !inside(msg_off, msg_len) {
        return Err(QuoteError::BadEd25519Header);
    }
    if &d[pk_off..pk_off + PUBKEY_LEN] != signer.as_ref() {
        return Err(QuoteError::BadSignature);
    }
    if msg_len != message.len() || &d[msg_off..msg_off + msg_len] != message {
        return Err(QuoteError::BadSignature);
    }
    Ok(())
}

/// The quote's price, after checking its fields and that the instruction just
/// before the current one is a precompile verification of `signer` over this
/// quote's message. `instructions` must be the Instructions sysvar (the
/// account constraint pins its address; the checked loaders check it again).
pub fn verify(
    instructions: &AccountInfo,
    quote: &Quote,
    side: u8,
    signer: &Pubkey,
    program_id: &Pubkey,
    now: i64,
) -> std::result::Result<u64, QuoteError> {
    quote.check_fields(side, now)?;
    let current = load_current_index_checked(instructions).map_err(|_| QuoteError::NotEd25519)?;
    let prev = current.checked_sub(1).ok_or(QuoteError::NotEd25519)?;
    let ix = load_instruction_at_checked(prev as usize, instructions).map_err(|_| QuoteError::NotEd25519)?;
    check_ed25519_instruction(&ix, signer, &quote.message(program_id))?;
    Ok(quote.price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    const PROGRAM: Pubkey = Pubkey::new_from_array([7; 32]);

    fn quote() -> Quote {
        Quote { position_id: [9; 32], side: BUY, price: 820_000, max_amount: 15_000_000, valid_until: 1_000 }
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[42; 32])
    }

    fn signer() -> Pubkey {
        Pubkey::new_from_array(key().verifying_key().to_bytes())
    }

    /// The precompile instruction a client builds: header, pubkey, signature, message.
    fn ed25519_ix(message: &[u8]) -> Instruction {
        let k = key();
        let sig = k.sign(message).to_bytes();
        let pk_off = HEADER_LEN as u16;
        let sig_off = pk_off + PUBKEY_LEN as u16;
        let msg_off = sig_off + SIGNATURE_LEN as u16;
        let mut d = vec![1u8, 0];
        for v in [sig_off, THIS_INSTRUCTION, pk_off, THIS_INSTRUCTION, msg_off, message.len() as u16, THIS_INSTRUCTION] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&k.verifying_key().to_bytes());
        d.extend_from_slice(&sig);
        d.extend_from_slice(message);
        Instruction { program_id: ed25519_program::ID, accounts: vec![], data: d }
    }

    fn check(ix: &Instruction) -> std::result::Result<(), QuoteError> {
        check_ed25519_instruction(ix, &signer(), &quote().message(&PROGRAM))
    }

    fn set_u16(ix: &mut Instruction, at: usize, v: u16) {
        ix.data[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn message_layout_is_fixed() {
        let m = quote().message(&PROGRAM);
        assert_eq!(m.len(), 106);
        assert_eq!(&m[..16], DOMAIN);
        assert_eq!(&m[16..48], PROGRAM.as_ref());
        assert_eq!(m[48], CLUSTER);
        assert_eq!(&m[49..81], &[9; 32]);
        assert_eq!(m[81], BUY);
        assert_eq!(u64::from_le_bytes(m[82..90].try_into().unwrap()), 820_000);
        assert_eq!(u64::from_le_bytes(m[90..98].try_into().unwrap()), 15_000_000);
        assert_eq!(i64::from_le_bytes(m[98..106].try_into().unwrap()), 1_000);
    }

    #[test]
    fn accepts_the_honest_instruction() {
        assert_eq!(check(&ed25519_ix(&quote().message(&PROGRAM))), Ok(()));
    }

    #[test]
    fn refuses_another_program() {
        let mut ix = ed25519_ix(&quote().message(&PROGRAM));
        ix.program_id = Pubkey::new_from_array([1; 32]);
        assert_eq!(check(&ix), Err(QuoteError::NotEd25519));
    }

    #[test]
    fn refuses_accounts_on_the_precompile() {
        let mut ix = ed25519_ix(&quote().message(&PROGRAM));
        ix.accounts.push(anchor_lang::solana_program::instruction::AccountMeta::new_readonly(PROGRAM, false));
        assert_eq!(check(&ix), Err(QuoteError::BadEd25519Header));
    }

    #[test]
    fn refuses_zero_or_several_signatures() {
        for n in [0u8, 2] {
            let mut ix = ed25519_ix(&quote().message(&PROGRAM));
            ix.data[0] = n;
            assert_eq!(check(&ix), Err(QuoteError::BadEd25519Header), "num_signatures {n}");
        }
    }

    #[test]
    fn refuses_short_data() {
        let mut ix = ed25519_ix(&quote().message(&PROGRAM));
        ix.data.truncate(HEADER_LEN - 1);
        assert_eq!(check(&ix), Err(QuoteError::BadEd25519Header));
    }

    /// Each instruction-index field pointing anywhere but this instruction.
    #[test]
    fn refuses_data_read_from_another_instruction() {
        for at in [4usize, 8, 14] {
            for v in [0u16, 1, 2, 0xfffe] {
                let mut ix = ed25519_ix(&quote().message(&PROGRAM));
                set_u16(&mut ix, at, v);
                assert_eq!(check(&ix), Err(QuoteError::BadEd25519Header), "field {at} = {v}");
            }
        }
    }

    /// Offsets into the header, or running past the end.
    #[test]
    fn refuses_offsets_outside_the_data() {
        let base = ed25519_ix(&quote().message(&PROGRAM));
        let len = base.data.len() as u16;
        for (at, v) in [(2usize, 0u16), (2, 15), (2, len - 63), (6, 0), (6, len - 31), (10, 0), (10, len - 105), (12, 200), (2, u16::MAX)] {
            let mut ix = base.clone();
            set_u16(&mut ix, at, v);
            assert_eq!(check(&ix), Err(QuoteError::BadEd25519Header), "field {at} = {v}");
        }
    }

    #[test]
    fn refuses_another_signer() {
        let ix = ed25519_ix(&quote().message(&PROGRAM));
        let other = Pubkey::new_from_array([3; 32]);
        assert_eq!(check_ed25519_instruction(&ix, &other, &quote().message(&PROGRAM)), Err(QuoteError::BadSignature));
    }

    /// The Relay bug: the key and message the program expects sit in the data,
    /// but the offsets the precompile follows name other bytes.
    #[test]
    fn refuses_offsets_that_point_away_from_the_expected_bytes() {
        let honest = quote().message(&PROGRAM);
        let mut ix = ed25519_ix(&honest);
        // Append an attacker's key and message; aim the offsets at them.
        let pk_off = ix.data.len() as u16;
        ix.data.extend_from_slice(&[5; 32]);
        let msg_off = ix.data.len() as u16;
        ix.data.extend_from_slice(&[6; MESSAGE_LEN]);
        set_u16(&mut ix, 6, pk_off);
        assert_eq!(check(&ix), Err(QuoteError::BadSignature));
        let mut ix = ed25519_ix(&honest);
        let other_msg = ix.data.len() as u16;
        ix.data.extend_from_slice(&[6; MESSAGE_LEN]);
        set_u16(&mut ix, 10, other_msg);
        assert_eq!(check(&ix), Err(QuoteError::BadSignature));
        // Both moved together, as a forger would.
        let mut ix = ed25519_ix(&honest);
        ix.data.extend_from_slice(&[5; 32]);
        ix.data.extend_from_slice(&[6; MESSAGE_LEN]);
        set_u16(&mut ix, 6, pk_off);
        set_u16(&mut ix, 10, msg_off);
        assert_eq!(check(&ix), Err(QuoteError::BadSignature));
    }

    #[test]
    fn refuses_any_other_message() {
        let q = quote();
        let variants = [
            Quote { price: q.price + 1, ..q },
            Quote { max_amount: q.max_amount + 1, ..q },
            Quote { valid_until: q.valid_until + 1, ..q },
            Quote { side: SELL, ..q },
            Quote { position_id: [8; 32], ..q },
        ];
        for v in variants {
            let ix = ed25519_ix(&v.message(&PROGRAM));
            assert_eq!(check(&ix), Err(QuoteError::BadSignature), "{v:?}");
        }
        // Signed for another deployment.
        let ix = ed25519_ix(&q.message(&Pubkey::new_from_array([8; 32])));
        assert_eq!(check(&ix), Err(QuoteError::BadSignature));
        // A longer message with the right prefix.
        let mut long = q.message(&PROGRAM).to_vec();
        long.push(0);
        assert_eq!(check(&ed25519_ix(&long)), Err(QuoteError::BadSignature));
    }

    #[test]
    fn field_checks_mirror_the_oracle() {
        let q = quote();
        assert_eq!(q.check_fields(BUY, 1_000), Ok(()));
        assert_eq!(q.check_fields(SELL, 1_000), Err(QuoteError::WrongSide));
        assert_eq!(q.check_fields(BUY, 1_001), Err(QuoteError::QuoteExpired));
        assert_eq!(q.check_fields(BUY, 1_000 - MAX_VALIDITY - 1), Err(QuoteError::QuoteTooLong));
        assert_eq!(q.check_fields(BUY, 1_000 - MAX_VALIDITY), Ok(()));
        assert_eq!(Quote { price: 0, ..q }.check_fields(BUY, 1_000), Err(QuoteError::PriceOutOfRange));
        assert_eq!(Quote { price: MAX_PRICE + 1, ..q }.check_fields(BUY, 1_000), Err(QuoteError::PriceOutOfRange));
        assert_eq!(Quote { price: MAX_PRICE, ..q }.check_fields(BUY, 1_000), Ok(()));
    }
}
