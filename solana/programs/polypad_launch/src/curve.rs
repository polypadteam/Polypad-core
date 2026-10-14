//! The coin's curve as one Meteora DBC segment (docs/solana/METEORA.md §2).
//!
//! The EVM `BondingCurve` is constant product over a phantom pToken reserve:
//! `(phantom + q) · y = phantom · S`, 5/7 of the supply sold, graduation at
//! `q* = 2.5 · phantom` shares. In DBC's terms that is exactly one
//! concentrated-liquidity segment with `L = sqrt(phantom · S)` and
//! `√P0 = sqrt(phantom / S)` (sqrt prices in Q64.64, `quote = L·Δ√P / 2^128`),
//! ending where the segment holds `q*` of quote. DBC then derives the
//! migration price, the base sold and the base that seeds the pool; this
//! module replays the same integer math (same rounding) so `launch` can pass
//! the post-migration supply DBC requires and then check that DBC computed
//! exactly these numbers.

use ruint::aliases::U256;

/// 1,000,000,000 coins at 6 decimals.
pub const SUPPLY: u64 = 1_000_000_000_000_000;
pub const DECIMALS: u8 = 6;
/// DBC's full price range (`constants.rs`); the migrated pool spans it.
pub const MIN_SQRT_PRICE: u128 = 4_295_048_016;
pub const MAX_SQRT_PRICE: u128 = 79_226_673_521_066_979_257_578_248_091;

/// Everything `create_config` takes or computes for one coin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Curve {
    /// Virtual pToken reserve, base units: the EVM `phantom`.
    pub phantom: u64,
    /// pToken raised at graduation (`migration_quote_threshold`): the EVM `target()`.
    pub threshold: u64,
    pub sqrt_start_price: u128,
    /// The one segment: its end price and liquidity.
    pub sqrt_end_price: u128,
    pub liquidity: u128,
    /// What DBC will compute from the above.
    pub migration_sqrt_price: u128,
    pub swap_base_amount: u64,
    pub migration_base_threshold: u64,
    /// `swap_base_amount + migration_base_threshold`: DBC burns the rest of
    /// the supply at migration (about 4/49, the EVM's burn to DEAD).
    pub post_migration_supply: u64,
}

fn u(x: u128) -> U256 {
    U256::from(x)
}

fn to_u128(x: U256) -> Option<u128> {
    u128::try_from(x).ok()
}

fn to_u64(x: U256) -> Option<u64> {
    u64::try_from(x).ok()
}

/// Floor square root (Newton from a power of two above the root).
pub fn isqrt(n: U256) -> U256 {
    if n.is_zero() {
        return n;
    }
    let mut x = U256::from(1u8) << n.bit_len().div_ceil(2);
    loop {
        let y = (x + n / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// `L · (hi − lo) / (lo · hi)`, rounded up: base between two sqrt prices
/// (DBC `get_delta_amount_base_unsigned_256`, `Rounding::Up`).
fn delta_base_up(lo: u128, hi: u128, l: u128) -> Option<U256> {
    let num = u(l).checked_mul(u(hi - lo))?;
    let den = u(lo).checked_mul(u(hi))?;
    Some(num.div_ceil(den))
}

/// `L · (hi − lo) / 2^128`, rounded up (DBC `get_delta_amount_quote_unsigned_256`).
fn delta_quote_up(lo: u128, hi: u128, l: u128) -> Option<U256> {
    let num = u(l).checked_mul(u(hi - lo))?;
    Some(num.div_ceil(U256::from(1u8) << 128usize))
}

/// The curve for a launch at `price` (6 decimals) with graduation at
/// `grad_usd` (USDC base units). None if any step overflows or leaves
/// DBC's price range, which the factory's bounds rule out.
pub fn curve(grad_usd: u64, price: u64) -> Option<Curve> {
    // EVM: phantom = floor(gradUsd · 2e6 / (price · 5)); target = phantom · 5 / 2.
    let phantom = (grad_usd as u128).checked_mul(2_000_000)? / (price as u128).checked_mul(5)?;
    let phantom = u64::try_from(phantom).ok().filter(|p| *p > 0)?;
    let threshold = u64::try_from(phantom as u128 * 5 / 2).ok()?;
    let s = u(SUPPLY as u128);

    // √P0 = isqrt(phantom · 2^128 / S); L = isqrt(phantom · S · 2^128).
    let sqrt_start_price = to_u128(isqrt((u(phantom as u128) << 128usize) / s))?;
    let liquidity = to_u128(isqrt(u(phantom as u128).checked_mul(s)? << 128usize))?;
    // The segment ends where it holds q* rounded up: √P0 + ceil(q* · 2^128 / L),
    // the exact 3.5 · √P0. Ending there (not at the maximum price) keeps
    // DBC's 25% swap buffer from minting supply the curve can never sell.
    let step = to_u128((u(threshold as u128) << 128usize).div_ceil(u(liquidity)))?;
    let sqrt_end_price = sqrt_start_price.checked_add(step)?;
    if sqrt_start_price < MIN_SQRT_PRICE || sqrt_end_price >= MAX_SQRT_PRICE {
        return None;
    }

    // DBC `get_migration_threshold_price`: inside the segment, the price
    // after exactly q* of quote in (rounded down); at its end otherwise.
    let whole = delta_quote_up(sqrt_start_price, sqrt_end_price, liquidity)?;
    let migration_sqrt_price = if whole > u(threshold as u128) {
        sqrt_start_price.checked_add(to_u128((u(threshold as u128) << 128usize) / u(liquidity))?)?
    } else {
        sqrt_end_price
    };
    // `get_base_token_for_swap`: base sold from √P0 to the migration price.
    let swap_base_amount = to_u64(delta_base_up(sqrt_start_price, migration_sqrt_price, liquidity)?)?;
    // `get_included_protocol_fee_migration_amounts_1` (no migration fee):
    // the base that pairs with q* in a full-range position at that price.
    let pool_liquidity = to_u128((u(threshold as u128) << 128usize) / u(migration_sqrt_price - MIN_SQRT_PRICE))?;
    let migration_base_threshold = to_u64(delta_base_up(migration_sqrt_price, MAX_SQRT_PRICE, pool_liquidity)?)?;
    let post_migration_supply = swap_base_amount.checked_add(migration_base_threshold)?;
    if post_migration_supply > SUPPLY {
        return None;
    }
    Some(Curve {
        phantom,
        threshold,
        sqrt_start_price,
        sqrt_end_price,
        liquidity,
        migration_sqrt_price,
        swap_base_amount,
        migration_base_threshold,
        post_migration_supply,
    })
}

/// The liquidity DBC's migration puts in the fee vault's position: the
/// raise less Meteora's migration cut (`protocol_fee_bps` of it, rounded
/// down), over the full range at the migration price
/// (`get_initial_liquidity_from_delta_quote`, rounded down). The base side
/// never binds: the curve vault still holds the whole unsold supply then.
/// A value fixed at launch, so liquidity anyone adds to the pool later
/// cannot change what `graduate` expects.
pub fn migrated_liquidity(threshold: u64, migration_sqrt_price: u128, protocol_fee_bps: u16) -> Option<u128> {
    let fee = threshold as u128 * protocol_fee_bps as u128 / 10_000;
    let quote = threshold as u128 - fee;
    to_u128((u(quote) << 128usize) / u(migration_sqrt_price.checked_sub(MIN_SQRT_PRICE).filter(|d| *d > 0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAD: u64 = 6_000_000_000;

    /// The worked examples in METEORA.md §2 (60c and 5c, gradUsd $6,000).
    #[test]
    fn reproduces_the_doc_table() {
        let c = curve(GRAD, 600_000).unwrap();
        assert_eq!(c.phantom, 4_000_000_000);
        assert_eq!(c.threshold, 10_000_000_000);
        assert_eq!(c.sqrt_start_price, 36_893_488_147_419_103);
        assert_eq!(c.sqrt_end_price, 129_127_208_515_966_862);
        assert_eq!(c.liquidity, 36_893_488_147_419_103_232_000_000_000_000);
        assert_eq!(c.migration_sqrt_price, 129_127_208_515_966_861);
        assert_eq!(c.swap_base_amount, 714_285_714_285_715);
        assert_eq!(c.migration_base_threshold, 204_081_639_440_922);
        assert_eq!(c.post_migration_supply, 918_367_353_726_637);

        let c = curve(GRAD, 50_000).unwrap();
        assert_eq!(c.phantom, 48_000_000_000);
        assert_eq!(c.threshold, 120_000_000_000);
        assert_eq!(c.sqrt_start_price, 127_802_791_879_540_120);
        assert_eq!(c.sqrt_end_price, 447_309_771_578_390_423);
        assert_eq!(c.liquidity, 127_802_791_879_540_120_812_585_215_911_708);
        assert_eq!(c.migration_sqrt_price, 447_309_771_578_390_422);
        assert_eq!(c.swap_base_amount, 714_285_714_285_715);
        assert_eq!(c.migration_base_threshold, 204_081_634_611_492);
        assert_eq!(c.post_migration_supply, 918_367_348_897_207);
    }

    /// Across the whole band and the factory's gradUsd range, the shares sold,
    /// pooled and burned stay the EVM's 5/7, 10/49 and 4/49 of supply.
    #[test]
    fn shares_match_the_evm_everywhere() {
        let sold = SUPPLY as u128 * 5 / 7;
        let pooled = SUPPLY as u128 * 10 / 49;
        for grad in [1_000_000_000u64, GRAD, 1_000_000_000_000] {
            for price in [1_000u64, 1_234, 50_000, 500_000, 600_000, 987_654, 999_000] {
                let c = curve(grad, price).unwrap_or_else(|| panic!("{grad} {price}"));
                let near = |a: u64, b: u128| (a as i128 - b as i128).unsigned_abs() * 100_000 < b;
                assert!(near(c.swap_base_amount, sold), "{grad} {price} {c:?}");
                assert!(near(c.migration_base_threshold, pooled), "{grad} {price} {c:?}");
                assert!(c.migration_sqrt_price <= c.sqrt_end_price);
                assert_eq!(c.threshold, (c.phantom as u128 * 5 / 2) as u64);
            }
        }
    }

    /// The vault's locked liquidity after migration at 60c, $6,000, with
    /// Meteora's 0.2% cut: what the real DBC put in the position.
    #[test]
    fn migrated_liquidity_is_dbcs() {
        let c = curve(GRAD, 600_000).unwrap();
        assert_eq!(migrated_liquidity(c.threshold, c.migration_sqrt_price, 20), Some(26_299_787_425_590_406_213_376_405_164_237));
        assert_eq!(migrated_liquidity(c.threshold, MIN_SQRT_PRICE, 20), None);
    }

    #[test]
    fn isqrt_is_the_floor_root() {
        for n in [0u128, 1, 2, 3, 4, 15, 16, 17, u64::MAX as u128, u128::MAX] {
            let r = isqrt(u(n));
            assert!(r * r <= u(n) && (r + U256::from(1u8)) * (r + U256::from(1u8)) > u(n), "{n}");
        }
        let big = U256::MAX;
        let r = isqrt(big);
        assert!(r == U256::from(u128::MAX));
    }
}
