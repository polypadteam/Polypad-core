//! Accounts. `Factory` holds what `LaunchFactory` kept contract-wide; one
//! `Launch` per coin holds the factory's registration and the FeeVault's
//! per-coin state; `Owed` is the FeeVault's `owed[asset][payee]`. Seeds use
//! distinct literal prefixes and fixed-length parts only.

use anchor_lang::prelude::*;

pub const FACTORY_SEED: &[u8] = b"factory";
/// Signs as DBC `fee_claimer` and `leftover_receiver`, owns the locked pool
/// position and every fee token account. Holds no data.
pub const FEE_VAULT_SEED: &[u8] = b"fee_vault";
pub const DBC_CONFIG_SEED: &[u8] = b"dbc_config";
pub const LAUNCH_SEED: &[u8] = b"launch";
pub const OWED_SEED: &[u8] = b"owed";
/// The platform's `Owed` has its own prefix, so it is never the same account
/// as a payee's even when the owner makes the platform a creator's wallet.
pub const PLATFORM_OWED_SEED: &[u8] = b"platform_owed";

pub const BPS: u64 = 10_000;

/// `LaunchFactory` bounds: the curve fee at most 2% (and at least DBC's
/// 0.25% floor), the creator side 30-80% of each fee.
pub const MIN_CURVE_FEE_BPS: u16 = 25;
pub const MAX_CURVE_FEE_BPS: u16 = 200;
pub const MIN_CREATOR_SHARE_BPS: u16 = 3_000;
pub const MAX_CREATOR_SHARE_BPS: u16 = 8_000;
/// The pool fee is fixed by the DAMM v2 config graduation uses (`Hv8L…`, 1%).
pub const POOL_FEE_BPS: u16 = 100;
/// gradUsd bounds, USDC base units ($1,000 to $1,000,000).
pub const MIN_GRAD_USD: u64 = 1_000_000_000;
pub const MAX_GRAD_USD: u64 = 1_000_000_000_000;
pub const DEFAULT_GRAD_USD: u64 = 6_000_000_000;

/// Fees fixed per coin at launch (the EVM `Fees` struct). The pool fee is in
/// bps here (100 = 1%), not Uniswap v4 hundredths of a bip.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct Fees {
    pub curve_fee_bps: u16,
    pub curve_creator_share_bps: u16,
    pub pool_fee_bps: u16,
    pub pool_creator_share_bps: u16,
}

impl Fees {
    pub const DEFAULT: Fees =
        Fees { curve_fee_bps: 140, curve_creator_share_bps: 5_000, pool_fee_bps: POOL_FEE_BPS, pool_creator_share_bps: 5_000 };

    pub fn valid(&self) -> bool {
        let share = MIN_CREATOR_SHARE_BPS..=MAX_CREATOR_SHARE_BPS;
        (MIN_CURVE_FEE_BPS..=MAX_CURVE_FEE_BPS).contains(&self.curve_fee_bps)
            && self.pool_fee_bps == POOL_FEE_BPS
            && share.contains(&self.curve_creator_share_bps)
            && share.contains(&self.pool_creator_share_bps)
    }
}

/// `LaunchFactory` state. PDA `["factory"]`; it is also the exchange's
/// `factory` signer, so it alone may create markets there.
#[account]
#[derive(InitSpace, Debug)]
pub struct Factory {
    pub owner: Pubkey,
    pub pending_owner: Pubkey,
    /// Pays holders: may move a coin's holder share to `holder_payout`, nothing else.
    pub keeper: Pubkey,
    /// Receives the platform side of every fee (as `Owed`).
    pub platform: Pubkey,
    /// The wallet the keeper pays holders from (D3). Set by the owner, so a
    /// stolen keeper key cannot send the holders' share anywhere else.
    pub holder_payout: Pubkey,
    pub grad_usd: u64,
    pub fees: Fees,
    pub bump: u8,
    pub vault_bump: u8,
}

/// One coin: what `LaunchFactory` registered and what the FeeVault keeps
/// per coin. PDA `["launch", base_mint]`. Its existence is what makes a
/// DBC pool a Polypad coin (METEORA.md M8): the indexer and every
/// instruction here accept only `pool` and `dbc_config` from this record.
#[account]
#[derive(InitSpace, Debug)]
pub struct Launch {
    pub base_mint: Pubkey,
    pub position_id: [u8; 32],
    pub ptoken_mint: Pubkey,
    /// PDA `["dbc_config", base_mint]`, created by `launch` and by nothing else.
    pub dbc_config: Pubkey,
    /// The DBC pool `launch` required in its transaction. Other pools on the
    /// same config can exist (DBC lets anyone open one); they are not this coin.
    pub pool: Pubkey,
    /// The quote's `valid_until`: the pool must be opened by then (in the
    /// launch transaction or its bundle), or the curve, sized at that quote's
    /// price, is stale. A pool opened later never graduates or collects
    /// here, and anyone can close the record (`expire_launch`).
    pub pool_deadline: i64,
    pub creator: Pubkey,
    /// Receives the creator side less the holders' part. Two-step transfer.
    pub payee: Pubkey,
    pub pending_payee: Pubkey,
    /// Share of the creator side kept for holders (D3), fixed at launch.
    pub holders_bps: u16,
    pub fees: Fees,
    /// The quoted BUY price the curve was sized at, and the curve's numbers.
    pub price: u64,
    pub phantom: u64,
    pub threshold: u64,
    pub launched_at: i64,
    /// Set by `graduate` once DBC has migrated the curve into DAMM v2.
    pub graduated: bool,
    /// The DAMM v2 config the pool was created under (always `Hv8L…`).
    pub damm_config: Pubkey,
    pub damm_pool: Pubkey,
    pub position: Pubkey,
    pub position_nft_account: Pubkey,
    /// The fee vault position's permanently locked liquidity: exactly what
    /// DBC's migration put in it.
    pub locked_liquidity: u128,
    /// Holders' pToken not yet moved to the payout wallet.
    pub holders_owed: u64,
    /// pToken fees collected, before the split, per source.
    pub curve_fees: u64,
    pub pool_fees: u64,
    pub bump: u8,
}

/// Tokens owed to one payee (a creator's payee or the platform), across
/// every coin of that market: the EVM `owed[asset][payee]`, so fees already
/// owed stay with the old payee when a coin's payee changes. PDA
/// `["owed", mint, payee]` for a payee, `["platform_owed", mint, platform]`
/// for the platform. `mint` is a pToken, or for fees swept from look-alike
/// pools whatever those pools paid in.
#[account]
#[derive(InitSpace, Debug)]
pub struct Owed {
    pub mint: Pubkey,
    pub payee: Pubkey,
    pub amount: u64,
    pub bump: u8,
    pub platform: bool,
}

impl Owed {
    pub fn seed(&self) -> &'static [u8] {
        if self.platform {
            PLATFORM_OWED_SEED
        } else {
            OWED_SEED
        }
    }
}

/// A fee split as the EVM `_payFee`/`_split` then `FeeVault.deposit` do it:
/// the creator side floored from the fee, the holders' part floored from
/// the creator side; the platform takes the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Split {
    pub payee: u64,
    pub holders: u64,
    pub platform: u64,
}

pub fn split(amount: u64, creator_share_bps: u16, holders_bps: u16) -> Split {
    let creator = (amount as u128 * creator_share_bps as u128 / BPS as u128) as u64;
    let holders = (creator as u128 * holders_bps as u128 / BPS as u128) as u64;
    Split { payee: creator - holders, holders, platform: amount - creator }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_matches_the_evm_rounding() {
        assert_eq!(split(1_000, 5_000, 0), Split { payee: 500, holders: 0, platform: 500 });
        assert_eq!(split(1_001, 5_000, 2_500), Split { payee: 375, holders: 125, platform: 501 });
        assert_eq!(split(7, 3_000, 10_000), Split { payee: 0, holders: 2, platform: 5 });
        assert_eq!(split(u64::MAX, 8_000, 10_000).platform + split(u64::MAX, 8_000, 10_000).holders, u64::MAX);
        for a in [0u64, 1, 99, 12_345_678] {
            let s = split(a, 6_100, 3_300);
            assert_eq!(s.payee + s.holders + s.platform, a);
        }
    }

    #[test]
    fn fee_bounds_are_the_factorys() {
        assert!(Fees::DEFAULT.valid());
        assert!(!Fees { curve_fee_bps: 24, ..Fees::DEFAULT }.valid());
        assert!(!Fees { curve_fee_bps: 201, ..Fees::DEFAULT }.valid());
        assert!(!Fees { pool_fee_bps: 125, ..Fees::DEFAULT }.valid());
        assert!(!Fees { curve_creator_share_bps: 2_999, ..Fees::DEFAULT }.valid());
        assert!(!Fees { pool_creator_share_bps: 8_001, ..Fees::DEFAULT }.valid());
        assert!(Fees { curve_fee_bps: 200, curve_creator_share_bps: 8_000, pool_creator_share_bps: 3_000, ..Fees::DEFAULT }.valid());
    }
}
