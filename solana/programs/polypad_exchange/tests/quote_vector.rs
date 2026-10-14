//! The cross-language check: `tests/fixtures/quote_v1.json` is written by
//! lib/oracle/solana-quote.ts (the pricer's side) and its own test checks the
//! TypeScript still produces it. This checks `quote.rs` (the program's side)
//! builds the same message from the same quote and accepts the same
//! precompile instruction, so the two cannot drift apart unnoticed.
//!
//! The fixture is a devnet quote (cluster byte 0), so this only runs without
//! the `mainnet` feature.
#![cfg(not(feature = "mainnet"))]

use std::str::FromStr;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _};
use polypad_exchange::quote::{check_ed25519_instruction, Quote, BUY, CLUSTER, MESSAGE_LEN};
use solana_sdk_ids::ed25519_program;

const FIXTURE: &str = include_str!("fixtures/quote_v1.json");

/// A string field of the fixture. The file is flat and machine-written, so a
/// few lines here stand in for a JSON dependency the program does not need.
fn field(key: &str) -> &'static str {
    let pat = format!("\"{key}\": ");
    let at = FIXTURE.find(&pat).unwrap_or_else(|| panic!("fixture has no {key}")) + pat.len();
    let rest = &FIXTURE[at..];
    match rest.strip_prefix('"') {
        Some(s) => &s[..s.find('"').unwrap()],
        None => rest[..rest.find([',', '\n']).unwrap()].trim(),
    }
}

fn hex(key: &str) -> Vec<u8> {
    let s = field(key);
    assert!(s.len() % 2 == 0, "{key}: odd-length hex");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn bytes32(key: &str) -> [u8; 32] {
    hex(key).try_into().unwrap_or_else(|_| panic!("{key}: not 32 bytes"))
}

/// The decimal CLOB token id as the uint256's 32 big-endian bytes, worked out
/// here from the decimal string rather than taken from the fixture's hex.
fn decimal_to_be(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for c in s.bytes() {
        assert!(c.is_ascii_digit(), "position id is not decimal: {s}");
        let mut carry = (c - b'0') as u32;
        for b in out.iter_mut().rev() {
            let v = *b as u32 * 10 + carry;
            *b = v as u8;
            carry = v >> 8;
        }
        assert_eq!(carry, 0, "position id does not fit in 256 bits: {s}");
    }
    out
}

fn quote() -> Quote {
    assert_eq!(field("side"), "BUY");
    Quote {
        position_id: decimal_to_be(field("position_id")),
        side: BUY,
        price: field("price").parse().unwrap(),
        max_amount: field("max_amount").parse().unwrap(),
        valid_until: field("valid_until").parse().unwrap(),
    }
}

fn program() -> Pubkey {
    Pubkey::from_str(field("program_id")).unwrap()
}

fn signer() -> Pubkey {
    Pubkey::new_from_array(bytes32("public_key"))
}

#[test]
fn fixture_inputs_agree_with_each_other() {
    assert_eq!(field("cluster_byte"), CLUSTER.to_string());
    assert_eq!(field("side_byte"), BUY.to_string());
    assert_eq!(decimal_to_be(field("position_id")), bytes32("position_id_hex"));
    // The key is the one quote.rs's own tests use: 32 bytes of 42.
    let key = SigningKey::from_bytes(&bytes32("seed"));
    assert_eq!(bytes32("seed"), [42; 32]);
    assert_eq!(key.verifying_key().to_bytes(), bytes32("public_key"));
    assert_eq!(signer(), Pubkey::from_str(field("public_key_base58")).unwrap());
}

#[test]
fn quote_rs_builds_the_same_message() {
    let m = quote().message(&program());
    assert_eq!(m.len(), MESSAGE_LEN);
    assert_eq!(m.to_vec(), hex("message"));
    // message_sha256 (the fill PDA seed) is checked when the program grows a
    // hasher; the crate has none yet, and the seed is only sha256 of these bytes.
}

#[test]
fn the_signature_is_the_signers_over_that_message() {
    let key = SigningKey::from_bytes(&bytes32("seed"));
    let m = quote().message(&program());
    let sig: [u8; 64] = hex("signature").try_into().unwrap();
    // ed25519 is deterministic: dalek signs to the same bytes node:crypto did.
    assert_eq!(key.sign(&m).to_bytes(), sig);
    key.verifying_key().verify(&m, &Signature::from_bytes(&sig)).unwrap();
}

#[test]
fn check_accepts_the_typescript_instruction() {
    let ix = Instruction { program_id: ed25519_program::ID, accounts: vec![], data: hex("ed25519_instruction_data") };
    assert_eq!(check_ed25519_instruction(&ix, &signer(), &quote().message(&program())), Ok(()));
    // And still refuses it for anyone else's key or any other quote.
    let other = Pubkey::new_from_array([3; 32]);
    assert!(check_ed25519_instruction(&ix, &other, &quote().message(&program())).is_err());
    let moved = Quote { price: quote().price + 1, ..quote() };
    assert!(check_ed25519_instruction(&ix, &signer(), &moved.message(&program())).is_err());
}
