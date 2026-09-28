// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {Fees} from "../../src/Fees.sol";
import {PToken} from "../../src/PToken.sol";
import {StateLibrary} from "v4-core/src/libraries/StateLibrary.sol";
import {PoolIdLibrary, PoolKey} from "v4-core/src/types/PoolId.sol";

import {CurveHarnessBase} from "./CurveHarnessBase.sol";

/// @dev Price impact: bigger trades never get better prices, the spot price moves the
///      right way, and the pool opens where the curve closed.
contract CurvePriceImpactTest is CurveHarnessBase {
    using PoolIdLibrary for PoolKey;

    /// Coins out never fall as pToken in rises (exact, no tolerance).
    function testFuzz_curveOutIsMonotonic(uint256 pre, uint256 a, uint256 b) public {
        pre = bound(pre, 0, 9_000e6);
        if (pre >= 2e6) _curveBuy(alice, _shares(alice, (pre * 6) / 10), 0);
        if (curve.graduated()) return;
        a = bound(a, 1, 20_000e6);
        b = bound(b, a, 20_000e6);
        assertLe(curve.quoteBuy(a), curve.quoteBuy(b));
    }

    /// A bigger buy never pays a better average price. Fee rounding (floor of amount x
    /// bps) makes adjacent dust sizes differ by up to one fee unit, so compare with that
    /// allowance: avg(b) >= avg(a) less one unit of pToken on a's side.
    function testFuzz_biggerBuyNeverBetterAveragePrice(uint256 pre, uint256 a, uint256 b) public {
        pre = bound(pre, 0, 5_000e6);
        if (pre >= 2e6) _curveBuy(alice, _shares(alice, (pre * 6) / 10), 0);
        a = bound(a, 1e6, 5_000e6);
        b = bound(b, a, 5_000e6);
        uint256 outA = curve.quoteBuy(a);
        uint256 outB = curve.quoteBuy(b);
        // b / outB >= (a - 1) / outA  <=>  b * outA >= (a - 1) * outB
        assertGe(b * outA, (a - 1) * outB, "bigger buy got a better average price");
    }

    /// Selling more never gets a better average price.
    function testFuzz_biggerSellNeverBetterAveragePrice(uint256 bought, uint256 a, uint256 b) public {
        bought = bound(bought, 100e6, 9_000e6);
        (uint256 coins,) = _curveBuy(alice, _shares(alice, (bought * 6) / 10), 0);
        if (curve.graduated()) return;
        a = bound(a, 1e18, coins);
        b = bound(b, a, coins);
        uint256 outA = curve.quoteSell(a);
        uint256 outB = curve.quoteSell(b);
        // outB / b <= (outA + 2) / a: the curve rounds the gross down and the
        // fee (rounded up) can take one more unit off the smaller sell.
        assertLe(outB * a, (outA + 2) * b, "bigger sell got a better average price");
    }

    /// Exact curve price as a fraction (phantom + quote) / tokens; spotPrice() itself is
    /// too coarse (a handful of units) to see a small trade.
    function _curvePrice() internal view returns (uint256 num, uint256 den) {
        return (curve.phantom() + curve.trackedQuote(), curve.trackedTokens());
    }

    function _gt(uint256 n1, uint256 d1, uint256 n0, uint256 d0) internal pure returns (bool) {
        return n1 * d0 > n0 * d1;
    }

    function testFuzz_spotRisesOnBuyFallsOnSell(uint256 pIn, uint256 frac) public {
        pIn = bound(pIn, 1e3, 9_000e6);
        _shares(alice, 7_000e6);
        (uint256 n0, uint256 d0) = _curvePrice();
        (uint256 coins,) = _curveBuy(alice, pIn, 0);
        if (curve.graduated()) return;
        (uint256 n1, uint256 d1) = _curvePrice();
        assertTrue(_gt(n1, d1, n0, d0), "buy did not raise price");
        uint256 sellAmt = (coins * bound(frac, 1, 100)) / 100;
        vm.assume(curve.quoteSell(sellAmt) > 0);
        _curveSell(alice, sellAmt, 0);
        (uint256 n2, uint256 d2) = _curvePrice();
        assertTrue(_gt(n1, d1, n2, d2), "sell did not lower price");
        assertFalse(_gt(n0, d0, n2, d2), "price fell below where it started");
    }

    function _sqrtP() internal view returns (uint160 s) {
        (s,,,) = StateLibrary.getSlot0(poolManager, graduator.poolKey(address(coin)).toId());
    }

    /// Pool price moves the right way too (raw pToken, both directions).
    function testFuzz_poolSpotRisesOnBuyFallsOnSell(uint256 pIn) public {
        _graduateDirect();
        pIn = bound(pIn, 1e6, 20_000e6);
        uint256 have = _shares(alice, (pIn * 7) / 10 + 1e6);
        if (have < pIn) pIn = have;
        // Price as pToken per coin rises when sqrtP moves toward the pToken side.
        bool coinIs0 = address(coin) < address(p);
        uint160 s0 = _sqrtP();
        uint256 coins = _poolSwap(alice, address(p), pIn);
        uint160 s1 = _sqrtP();
        assertTrue(coinIs0 ? s1 > s0 : s1 < s0, "pool buy did not raise price");
        _poolSwap(alice, address(coin), coins);
        uint160 s2 = _sqrtP();
        assertTrue(coinIs0 ? s2 < s1 : s2 > s1, "pool sell did not lower price");
        // Fees stay in the pool: selling back does not push the price below the start.
        assertTrue(coinIs0 ? s2 >= s0 : s2 <= s0, "pool round trip left price lower");
    }

    /// The pool opens at the curve's final marginal price, for any launch price and fees.
    function testFuzz_poolOpensAtTheCurveFinalPrice(uint64 price, uint16 feeBps, uint24 poolFee) public {
        price = uint64(bound(price, exchange.minPrice(), exchange.maxPrice()));
        feeBps = uint16(bound(feeBps, 1, factory.MAX_CURVE_FEE_BPS()));
        poolFee = uint24(bound(poolFee, 1, factory.MAX_POOL_FEE()));
        vm.prank(owner);
        factory.setFees(Fees(feeBps, 5_000, poolFee, 5_000));
        _post(ID, price);
        (Coin c, BondingCurve cv) = _launch(ID);
        PToken pt = exchange.pTokenOf(ID);

        uint256 finalPrice = ((cv.phantom() + cv.target()) * 1e18) / cv.reserved();
        uint256 pIn = _shares(bob, (cv.target() * price * 12) / 10 / 1e6 + 10e6);
        vm.startPrank(bob);
        pt.approve(address(cv), pIn);
        cv.buy(pIn, 0, bob);
        vm.stopPrank();
        assertTrue(cv.graduated());
        // Within 0.01%: the pool price is a rounded sqrt. At the tiniest prices
        // (a high launch price, a few hundred wei per coin) that is one unit.
        if (finalPrice < 1e6) assertApproxEqAbs(cv.spotPrice(), finalPrice, 1);
        else assertApproxEqRel(cv.spotPrice(), finalPrice, 1e14);
        assertEq(graduator.poolKey(address(c)).fee, poolFee);
    }
}
