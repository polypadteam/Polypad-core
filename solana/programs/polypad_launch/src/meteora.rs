//! Everything about Meteora DBC and DAMM v2 in one place: pinned addresses,
//! the per-coin config `launch` asks DBC for, and reads of DBC and DAMM v2
//! accounts at the offsets of their own (zero-copy, `repr(C)`) layouts.
//!
//! Bindings come from `declare_program!` over the IDLs vendored in
//! `solana/idls/` (DBC 0.2.1, cp-amm 0.2.5 = source 0.2.4), so every CPI
//! argument struct and account layout is Meteora's own, never hand-encoded.

use anchor_lang::prelude::*;

use crate::cp_amm;
use crate::curve::{Curve, DECIMALS, SUPPLY};
use crate::dynamic_bonding_curve as dbc;
use crate::error::LaunchError;
use crate::state::Fees;

pub const DBC_ID: Pubkey = dbc::ID;
pub const DAMM_ID: Pubkey = cp_amm::ID;
/// DBC's pool authority PDA (`["pool_authority"]`): owns curve vaults, creates the migrated pool.
pub const DBC_POOL_AUTHORITY: Pubkey = pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");
pub const DBC_EVENT_AUTHORITY: Pubkey = pubkey!("8Ks12pbrD6PXxfty1hVQiE9sc289zgU1zHkvXhrSdriF");
pub const DAMM_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");
pub const DAMM_EVENT_AUTHORITY: Pubkey = pubkey!("3rmHSu74h1ZcmAisVcWerTCiRDQbUrBKmcwptYGjHfet");
/// DAMM v2 static config every graduation uses: 1% base fee (plus a
/// dynamic fee capped at 20% of it), fees collected in token B (pToken)
/// only, pools creatable only by DBC's pool authority. `graduate` passes it
/// to DBC's migration itself and accepts a pool under no other config.
/// DBC accepts for `migration_fee_option = 2` only a DBC-only static config
/// with a flat 1% fee, and on 2026-10-13 `Hv8L…` was the only one on
/// mainnet (METEORA.md §4), so nobody can migrate a coin elsewhere first.
pub const DAMM_CONFIG: Pubkey = pubkey!("Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp");
/// FixedBps100: the migration fee option that sends graduation to `DAMM_CONFIG`.
pub const MIGRATION_FEE_OPTION: u8 = 2;
/// Position NFTs are Token-2022 mints.
pub const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const METADATA_PROGRAM: Pubkey = pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

/// DBC fees are numerators over 1e9.
pub const FEE_DENOMINATOR: u64 = 1_000_000_000;
/// Anti-sniper schedule (METEORA.md §6): the curve fee starts this much
/// higher and falls linearly to the coin's curve fee, one step a second,
/// over `SNIPER_SECONDS`. 25.4% → 1.4% over a minute at the default fee.
/// It taxes buys and sells alike, so a sniper's flip pays it twice.
pub const SNIPER_EXTRA_NUMERATOR: u64 = 240_000_000;
pub const SNIPER_SECONDS: u16 = 60;

/// DBC `MigrationProgress::CreatedPool`: the DAMM v2 pool exists.
pub const MIGRATION_CREATED_POOL: u8 = 3;

pub fn pool_address(config: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    let (hi, lo) = if base_mint > quote_mint { (base_mint, quote_mint) } else { (quote_mint, base_mint) };
    Pubkey::find_program_address(&[b"pool", config.as_ref(), hi.as_ref(), lo.as_ref()], &DBC_ID).0
}

/// The DAMM v2 pool of two mints under `damm_config` (cp-amm's seeds): one
/// pool per config and pair, so the config used decides the address.
pub fn damm_pool_address(damm_config: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    let (hi, lo) = if base_mint > quote_mint { (base_mint, quote_mint) } else { (quote_mint, base_mint) };
    Pubkey::find_program_address(&[b"pool", damm_config.as_ref(), hi.as_ref(), lo.as_ref()], &DAMM_ID).0
}

pub fn position_nft_account_address(nft_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"position_nft_account", nft_mint.as_ref()], &DAMM_ID).0
}

/// The per-coin DBC config: every number comes from the verified quote and
/// the factory, never from the caller.
pub fn config_parameters(c: &Curve, fees: &Fees) -> dbc::types::ConfigParameters {
    use dbc::types::*;
    let fee = fees.curve_fee_bps as u64 * (FEE_DENOMINATOR / 10_000);
    ConfigParameters {
        pool_fees: PoolFeeParameters {
            // FeeSchedulerLinear: fee = cliff − periods_passed · reduction,
            // never below `fee` after SNIPER_SECONDS.
            base_fee: BaseFeeParameters {
                cliff_fee_numerator: fee + SNIPER_EXTRA_NUMERATOR,
                first_factor: SNIPER_SECONDS,
                second_factor: 1,
                third_factor: SNIPER_EXTRA_NUMERATOR / SNIPER_SECONDS as u64,
                base_fee_mode: 0,
            },
            dynamic_fee: None,
        },
        // Fees in the quote (pToken) only, on buys and sells.
        collect_fee_mode: 0,
        // DAMM v2.
        migration_option: 1,
        // Timestamp: the schedule runs in seconds.
        activation_type: 1,
        token_type: 0,
        token_decimal: DECIMALS,
        // All LP to the partner (our fee vault), permanently locked.
        partner_liquidity_percentage: 0,
        partner_permanent_locked_liquidity_percentage: 100,
        creator_liquidity_percentage: 0,
        creator_permanent_locked_liquidity_percentage: 0,
        migration_quote_threshold: c.threshold,
        sqrt_start_price: c.sqrt_start_price,
        locked_vesting: LockedVestingParams {
            amount_per_period: 0,
            cliff_duration_from_migration_time: 0,
            frequency: 0,
            number_of_period: 0,
            cliff_unlock_amount: 0,
        },
        // FixedBps100: graduation goes to the 1% config `Hv8L…`.
        migration_fee_option: MIGRATION_FEE_OPTION,
        token_supply: Some(TokenSupplyParams { pre_migration_token_supply: SUPPLY, post_migration_token_supply: c.post_migration_supply }),
        // Meteora's creator role carries no money: every fee is the
        // partner's (our vault), which splits it per coin.
        creator_trading_fee_percentage: 0,
        // Immutable metadata, as the EVM metadataURI.
        token_update_authority: 1,
        migration_fee: MigrationFee { fee_percentage: 0, creator_fee_percentage: 0 },
        migrated_pool_fee: MigratedPoolFee { collect_fee_mode: 0, dynamic_fee: 0, pool_fee_bps: 0 },
        pool_creation_fee: 0,
        partner_liquidity_vesting_info: LiquidityVestingInfoParams {
            vesting_percentage: 0,
            bps_per_period: 0,
            number_of_periods: 0,
            cliff_duration_from_migration_time: 0,
            frequency: 0,
        },
        creator_liquidity_vesting_info: LiquidityVestingInfoParams {
            vesting_percentage: 0,
            bps_per_period: 0,
            number_of_periods: 0,
            cliff_duration_from_migration_time: 0,
            frequency: 0,
        },
        migrated_pool_base_fee_mode: 0,
        migrated_pool_market_cap_fee_scheduler_params: MigratedPoolMarketCapFeeSchedulerParams {
            number_of_period: 0,
            sqrt_price_step_bps: 0,
            scheduler_expiration_duration: 0,
            reduction_factor: 0,
        },
        // The creator's buy in the launch transaction pays the coin's curve
        // fee, not the schedule's start (DBC checks the pool creation and the
        // swap are both top-level instructions of that transaction).
        enable_first_swap_with_min_fee: true,
        compounding_fee_bps: 0,
        padding: [0; 2],
        curve: vec![LiquidityDistributionParameters { sqrt_price: c.sqrt_end_price, liquidity: c.liquidity }],
    }
}

/* ------------------------------------------------------------ reading */

fn read<T: bytemuck::Pod>(data: &[u8], at: usize) -> Result<T> {
    let end = at.checked_add(core::mem::size_of::<T>()).ok_or(LaunchError::WrongAccount)?;
    let bytes = data.get(at..end).ok_or(LaunchError::WrongAccount)?;
    Ok(bytemuck::pod_read_unaligned(bytes))
}

/// A Meteora account's data after checking its owner and discriminator.
fn data_of<'a, T: Discriminator + Owner>(ai: &'a AccountInfo) -> Result<std::cell::Ref<'a, &'a mut [u8]>> {
    require_keys_eq!(*ai.owner, T::owner(), LaunchError::WrongAccount);
    let d = ai.try_borrow_data()?;
    require!(d.len() >= 8 && &d[..8] == T::DISCRIMINATOR, LaunchError::WrongAccount);
    Ok(d)
}

macro_rules! field {
    ($data:expr, $ty:ty, $($f:tt)+) => {
        read(&$data, 8 + core::mem::offset_of!($ty, $($f)+))
    };
}

/// Every field of the config `launch` made, read back and compared with
/// what it asked for (`p`, from `config_parameters`) and what DBC must have
/// computed (`c`). Defence in depth against a DBC upgrade changing a layout
/// or a meaning under us (fees, fee mode, migration target, activation,
/// LP split, vesting, supply, curve): any difference refuses the launch.
/// Only `version` and `quote_token_flag` are left out: DBC sets them from
/// its own release and the quote mint, not from our parameters.
pub fn check_config(ai: &AccountInfo, quote_mint: &Pubkey, fee_vault: &Pubkey, c: &Curve, p: &dbc::types::ConfigParameters) -> Result<()> {
    use dbc::accounts::PoolConfig as P;
    let d = data_of::<P>(ai)?;
    let mut ok = true;
    // Each read is compared as the field's own type.
    macro_rules! eq {
        ($ty:ty, $want:expr, $($f:tt)+) => {{
            let got: $ty = field!(d, P, $($f)+)?;
            ok &= got == $want;
        }};
    }
    eq!(Pubkey, *quote_mint, quote_mint);
    eq!(Pubkey, *fee_vault, fee_claimer);
    eq!(Pubkey, *fee_vault, leftover_receiver);

    let b = &p.pool_fees.base_fee;
    eq!(u64, b.cliff_fee_numerator, pool_fees.base_fee.cliff_fee_numerator);
    eq!(u16, b.first_factor, pool_fees.base_fee.first_factor);
    eq!(u64, b.second_factor, pool_fees.base_fee.second_factor);
    eq!(u64, b.third_factor, pool_fees.base_fee.third_factor);
    eq!(u8, b.base_fee_mode, pool_fees.base_fee.base_fee_mode);
    eq!(u8, p.pool_fees.dynamic_fee.is_some() as u8, pool_fees.dynamic_fee.initialized);
    eq!(u8, p.collect_fee_mode, collect_fee_mode);
    eq!(u8, p.migration_option, migration_option);
    eq!(u8, p.activation_type, activation_type);
    eq!(u8, p.token_type, token_type);
    eq!(u8, p.token_decimal, token_decimal);

    eq!(u8, p.partner_liquidity_percentage, partner_liquidity_percentage);
    eq!(u8, p.partner_permanent_locked_liquidity_percentage, partner_permanent_locked_liquidity_percentage);
    eq!(u8, p.creator_liquidity_percentage, creator_liquidity_percentage);
    eq!(u8, p.creator_permanent_locked_liquidity_percentage, creator_permanent_locked_liquidity_percentage);
    for (v, info) in [
        (&p.partner_liquidity_vesting_info, core::mem::offset_of!(P, partner_liquidity_vesting_info)),
        (&p.creator_liquidity_vesting_info, core::mem::offset_of!(P, creator_liquidity_vesting_info)),
    ] {
        use dbc::types::LiquidityVestingInfo as I;
        let at = |o: usize| 8 + info + o;
        ok &= read::<u8>(&d, at(core::mem::offset_of!(I, vesting_percentage)))? == v.vesting_percentage
            && read::<u16>(&d, at(core::mem::offset_of!(I, bps_per_period)))? == v.bps_per_period
            && read::<u16>(&d, at(core::mem::offset_of!(I, number_of_periods)))? == v.number_of_periods
            && read::<u32>(&d, at(core::mem::offset_of!(I, frequency)))? == v.frequency
            && read::<u32>(&d, at(core::mem::offset_of!(I, cliff_duration_from_migration_time)))?
                == v.cliff_duration_from_migration_time;
    }
    let v = &p.locked_vesting;
    eq!(u64, v.amount_per_period, locked_vesting_config.amount_per_period);
    eq!(u64, v.cliff_duration_from_migration_time, locked_vesting_config.cliff_duration_from_migration_time);
    eq!(u64, v.frequency, locked_vesting_config.frequency);
    eq!(u64, v.number_of_period, locked_vesting_config.number_of_period);
    eq!(u64, v.cliff_unlock_amount, locked_vesting_config.cliff_unlock_amount);

    eq!(u8, p.migration_fee_option, migration_fee_option);
    eq!(u8, p.migration_fee.fee_percentage, migration_fee_percentage);
    eq!(u8, p.migration_fee.creator_fee_percentage, creator_migration_fee_percentage);
    eq!(u8, p.migrated_pool_fee.collect_fee_mode, migrated_collect_fee_mode);
    eq!(u8, p.migrated_pool_fee.dynamic_fee, migrated_dynamic_fee);
    eq!(u16, p.migrated_pool_fee.pool_fee_bps, migrated_pool_fee_bps);
    eq!(u8, p.migrated_pool_base_fee_mode, migrated_pool_base_fee_mode);
    eq!(u16, p.compounding_fee_bps, migrated_compounding_fee_bps);
    eq!([u8; 16], [0u8; 16], migrated_pool_base_fee_bytes);
    eq!(u8, p.creator_trading_fee_percentage, creator_trading_fee_percentage);
    eq!(u8, p.token_update_authority, token_update_authority);
    eq!(u8, p.enable_first_swap_with_min_fee as u8, enable_first_swap_with_min_fee);
    eq!(u64, p.pool_creation_fee, pool_creation_fee);

    let supply = p.token_supply.as_ref().ok_or(LaunchError::ConfigMismatch)?;
    eq!(u8, 1, fixed_token_supply_flag);
    eq!(u64, supply.pre_migration_token_supply, pre_migration_token_supply);
    eq!(u64, c.post_migration_supply, post_migration_token_supply);
    eq!(u128, c.sqrt_start_price, sqrt_start_price);
    // The curve is our one segment, then an empty slot (DBC's end marker).
    let seg = core::mem::size_of::<dbc::types::LiquidityDistributionConfig>();
    let curve = 8 + core::mem::offset_of!(P, curve);
    ok &= p.curve.len() == 1
        && read::<u128>(&d, curve)? == c.sqrt_end_price
        && read::<u128>(&d, curve + 16)? == c.liquidity
        && read::<u128>(&d, curve + seg)? == 0;

    // What DBC computed from the above.
    eq!(u64, c.threshold, migration_quote_threshold);
    eq!(u64, c.swap_base_amount, swap_base_amount);
    eq!(u64, c.migration_base_threshold, migration_base_threshold);
    eq!(u128, c.migration_sqrt_price, migration_sqrt_price);
    require!(ok, LaunchError::ConfigMismatch);
    Ok(())
}

/// The DBC pool's state `graduate` and `collect` rely on.
pub struct CurveState {
    pub config: Pubkey,
    pub base_mint: Pubkey,
    /// When the pool was opened (DBC activates a pool at creation).
    pub activation_point: u64,
    pub quote_reserve: u64,
    pub migrated: bool,
    /// Meteora's cut of the migrated liquidity, in bps (set per pool).
    pub migration_fee_bps: u16,
}

pub fn curve_state(ai: &AccountInfo) -> Result<CurveState> {
    use dbc::accounts::VirtualPool as V;
    let d = data_of::<V>(ai)?;
    let is_migrated: u8 = field!(d, V, pool_state.is_migrated)?;
    let progress: u8 = field!(d, V, pool_state.migration_progress)?;
    Ok(CurveState {
        config: field!(d, V, pool_state.config)?,
        base_mint: field!(d, V, pool_state.base_mint)?,
        activation_point: field!(d, V, pool_state.activation_point)?,
        quote_reserve: field!(d, V, pool_state.quote_reserve)?,
        migrated: is_migrated == 1 && progress == MIGRATION_CREATED_POOL,
        migration_fee_bps: field!(d, V, pool_state.protocol_liquidity_migration_fee_bps)?,
    })
}

/// Whether a DBC pool exists at this account: it may not yet (the launch's
/// second transaction has not landed) or ever.
pub fn is_curve_pool(ai: &AccountInfo) -> bool {
    use dbc::accounts::VirtualPool as V;
    *ai.owner == V::owner() && ai.try_borrow_data().is_ok_and(|d| d.len() >= 8 && &d[..8] == V::DISCRIMINATOR)
}

/// The migration price DBC stored on a coin's config.
pub fn migration_sqrt_price(ai: &AccountInfo) -> Result<u128> {
    use dbc::accounts::PoolConfig as P;
    let d = data_of::<P>(ai)?;
    field!(d, P, migration_sqrt_price)
}

/// The migrated DAMM v2 pool's mints. Its liquidity is never read: anyone
/// can add to it.
pub struct DammPool {
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
}

pub fn damm_pool(ai: &AccountInfo) -> Result<DammPool> {
    use cp_amm::accounts::Pool as P;
    let d = data_of::<P>(ai)?;
    Ok(DammPool {
        token_a_mint: field!(d, P, token_a_mint)?,
        token_b_mint: field!(d, P, token_b_mint)?,
    })
}

/// A DAMM v2 position: which pool and NFT, and how its liquidity is held.
pub struct DammPosition {
    pub pool: Pubkey,
    pub nft_mint: Pubkey,
    pub unlocked_liquidity: u128,
    pub vested_liquidity: u128,
    pub permanent_locked_liquidity: u128,
}

pub fn damm_position(ai: &AccountInfo) -> Result<DammPosition> {
    use cp_amm::accounts::Position as P;
    let d = data_of::<P>(ai)?;
    Ok(DammPosition {
        pool: field!(d, P, pool)?,
        nft_mint: field!(d, P, nft_mint)?,
        unlocked_liquidity: field!(d, P, unlocked_liquidity)?,
        vested_liquidity: field!(d, P, vested_liquidity)?,
        permanent_locked_liquidity: field!(d, P, permanent_locked_liquidity)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_addresses_are_the_derived_ones() {
        let pda = |seeds: &[&[u8]], p: &Pubkey| Pubkey::find_program_address(seeds, p).0;
        assert_eq!(DBC_POOL_AUTHORITY, pda(&[b"pool_authority"], &DBC_ID));
        assert_eq!(DBC_EVENT_AUTHORITY, pda(&[b"__event_authority"], &DBC_ID));
        assert_eq!(DAMM_POOL_AUTHORITY, pda(&[b"pool_authority"], &DAMM_ID));
        assert_eq!(DAMM_EVENT_AUTHORITY, pda(&[b"__event_authority"], &DAMM_ID));
        assert_eq!(DBC_ID, pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN"));
        assert_eq!(DAMM_ID, pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG"));
    }

    #[test]
    fn schedule_ends_at_the_curve_fee() {
        let c = crate::curve::curve(6_000_000_000, 600_000).unwrap();
        let p = config_parameters(&c, &Fees::DEFAULT);
        let b = p.pool_fees.base_fee;
        assert_eq!(b.cliff_fee_numerator, 254_000_000);
        assert_eq!(b.cliff_fee_numerator - b.third_factor * b.first_factor as u64, 14_000_000);
        assert_eq!(b.third_factor, 4_000_000);
    }
}
