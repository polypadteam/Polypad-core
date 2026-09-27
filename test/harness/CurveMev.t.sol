// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../../src/BondingCurve.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {Router} from "../../src/Router.sol";

import {CurveHarnessBase} from "./CurveHarnessBase.sol";

/// @dev Sandwiches and same-block round trips, on the curve and in the pool, in raw
///      pToken so exchange spreads do not hide anything.
contract CurveMevTest is CurveHarnessBase {
    uint256 internal constant BPS = 10_000;

    /// Same-block buy then sell on the curve never returns more pToken than went in.
    function testFuzz_curveRoundTripNeverProfits(uint256 pre, uint256 pIn) public {
        pre = bound(pre, 0, 8_000e6);
        if (pre >= 2e6) _curveBuy(alice, _shares(alice, (pre * 6) / 10), 0);
        if (curve.graduated()) return;
        pIn = bound(pIn, 1, 9_000e6);
        uint256 have = _shares(attacker, 7_000e6);
        if (pIn > have) pIn = have;
        uint256 p0 = p.balanceOf(attacker);
        uint256 coins;
        try this.extBuy(attacker, pIn) returns (uint256 c) {
            coins = c;
        } catch {
            return; // dust buy that rounds to zero coins
        }
        if (curve.graduated()) return; // covered by the pool test
        if (curve.quoteSell(coins) == 0) return;
        _curveSell(attacker, coins, 0);
        assertLe(p.balanceOf(attacker), p0, "curve round trip profited");
        _assertCurveSolvent();
    }

    function extBuy(address who, uint256 pIn) external returns (uint256 out) {
        require(msg.sender == address(this));
        (out,) = _curveBuy(who, pIn, 0);
    }

    /// Same-block buy then sell in the pool never profits.
    function testFuzz_poolRoundTripNeverProfits(uint256 pIn) public {
        _graduateDirect();
        pIn = bound(pIn, 1, 50_000e6);
        uint256 have = _shares(attacker, 40_000e6);
        if (pIn > have) pIn = have;
        uint256 p0 = p.balanceOf(attacker);
        uint256 coins = _poolSwap(attacker, address(p), pIn);
        if (coins == 0) return;
        _poolSwap(attacker, address(coin), coins);
        assertLe(p.balanceOf(attacker), p0, "pool round trip profited");
    }

    /// Classic sandwich on the curve. The attacker's profit can never exceed what the
    /// victim's shortfall (quoted minus received coins) is worth sold straight after an
    /// honest fill: the curve is path independent, so that is the whole of the value
    /// moved. With minOut at the fresh quote (tolerance 0) the victim's buy reverts and
    /// the attacker only pays fees.
    function testFuzz_curveSandwichBoundedByVictimShortfall(uint256 front, uint256 victimIn, uint256 tolBps)
        public
    {
        victimIn = bound(victimIn, 10e6, 3_000e6);
        front = bound(front, 1e6, 5_000e6);
        tolBps = bound(tolBps, 0, 2_000);
        uint256 vIn = _shares(victim, victimIn);
        if (vIn > victimIn) vIn = victimIn;
        uint256 aHave = _shares(attacker, 5_000e6);
        if (front > aHave) front = aHave;

        uint256 quoted = curve.quoteBuy(vIn);
        uint256 minOut = (quoted * (BPS - tolBps)) / BPS;
        // The honest state: victim alone.
        uint256 snap = vm.snapshotState();
        _curveBuy(victim, vIn, 0);
        uint256 v1 = curve.phantom() + curve.trackedQuote();
        uint256 t1 = curve.trackedTokens();
        vm.revertToState(snap);

        uint256 a0 = p.balanceOf(attacker);
        (uint256 aCoins,) = _curveBuy(attacker, front, 0);
        bool victimFilled;
        uint256 vCoins;
        if (!curve.graduated()) {
            vm.startPrank(victim);
            p.approve(address(curve), vIn);
            try curve.buy(vIn, minOut, victim) returns (uint256 o, uint256) {
                victimFilled = true;
                vCoins = o;
            } catch {}
            vm.stopPrank();
        }
        if (curve.graduated()) return;
        _curveSell(attacker, aCoins, 0);
        uint256 a1 = p.balanceOf(attacker);

        if (!victimFilled) {
            assertLe(a1, a0, "attacker profited without a victim fill");
            return;
        }
        assertGe(vCoins, minOut);
        if (tolBps == 0) assertEq(vCoins, quoted, "tight minOut let a worse fill through");
        if (a1 > a0) {
            uint256 shortfall = quoted - vCoins;
            // Shortfall sold into the honest post-trade curve, no fee: an upper bound.
            uint256 worth = (shortfall * v1) / (t1 + shortfall) + 1;
            assertLe(a1 - a0, worth, "sandwich extracted more than the victim lost");
        }
    }

    /// Router-level sandwich with a tight minCoins: the victim reverts, the attacker
    /// eats the round-trip cost (fees plus both exchange spreads).
    function testFuzz_routerSandwichWithTightMinCoinsIsUnprofitable(uint256 front, uint256 victimIn) public {
        victimIn = bound(victimIn, 10e6, 2_000e6);
        front = bound(front, 10e6, 4_000e6);
        uint256 expected = curve.quoteBuy(exchange.mintOut(px[ID], victimIn));
        uint256 u0 = usdg.balanceOf(attacker);
        (uint256 aCoins,) = _routerBuy(attacker, front, 0);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(victim);
        vm.expectRevert();
        router.buy(curve, victimIn, expected, victim, q, sig);

        _routerSell(attacker, aCoins, 0);
        assertLt(usdg.balanceOf(attacker), u0, "attacker profited");
        _assertRouterEmpty();
    }

    /// The attacker sells the curve out right before a victim's curve-sized buy: the
    /// victim's buy lands in the pool at a worse price and reverts on a minCoins taken
    /// from the curve quote.
    function test_graduationFrontRunIsCaughtByMinCoins() public {
        _curveBuy(alice, _shares(alice, 3_000e6), 0);
        uint256 victimUsd = 500e6;
        uint256 expected = curve.quoteBuy(exchange.mintOut(px[ID], victimUsd));

        // Attacker buys out the rest of the curve.
        _curveBuy(attacker, _shares(attacker, 8_000e6), 0);
        assertTrue(curve.graduated());

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(victim);
        vm.expectRevert();
        router.buy(curve, victimUsd, expected, victim, q, sig);
        // Loose minCoins: fills in the pool, fewer coins than the curve quote.
        (uint256 got,) = _routerBuy(victim, victimUsd, 0);
        assertLt(got, expected);
        _assertRouterEmpty();
    }

    /// Pool sandwich, raw pToken: profit never exceeds the victim's coin shortfall sold
    /// into the honest post-trade pool (fee added back, so an upper bound).
    function testFuzz_poolSandwichBoundedByVictimShortfall(uint256 front, uint256 victimIn, uint256 tolBps) public {
        _graduateDirect();
        victimIn = bound(victimIn, 10e6, 3_000e6);
        front = bound(front, 1e6, 10_000e6);
        tolBps = bound(tolBps, 0, 2_000);
        uint256 vIn = _shares(victim, victimIn);
        if (vIn > victimIn) vIn = victimIn;
        uint256 aHave = _shares(attacker, 10_000e6);
        if (front > aHave) front = aHave;

        uint256 quoted = router.quotePool(curve, true, vIn);
        uint256 minOut = (quoted * (BPS - tolBps)) / BPS;
        uint256 snap0 = vm.snapshotState();
        _poolSwap(victim, address(p), vIn);
        uint256 honest = vm.snapshotState();
        vm.revertToState(snap0);

        uint256 a0 = p.balanceOf(attacker);
        uint256 aCoins = _poolSwap(attacker, address(p), front);
        uint256 vCoins = _poolSwap(victim, address(p), vIn);
        _poolSwap(attacker, address(coin), aCoins);
        uint256 a1 = p.balanceOf(attacker);
        if (vCoins < minOut) return; // the victim's real transaction would have reverted
        if (tolBps == 0) assertGe(vCoins, quoted);
        if (a1 <= a0) return;
        uint256 shortfall = quoted - vCoins;
        vm.revertToState(honest);
        uint24 fee = graduator.poolKey(address(coin)).fee;
        uint256 worth = shortfall == 0 ? 0 : (router.quotePool(curve, false, shortfall) * 1e6) / (1e6 - fee) + 2;
        assertLe(a1 - a0, worth, "pool sandwich extracted more than the victim lost");
    }
}
