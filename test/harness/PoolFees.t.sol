// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {Fees} from "../../src/Fees.sol";
import {Graduator} from "../../src/Graduator.sol";
import {PToken} from "../../src/PToken.sol";

import {CurveHarnessBase} from "./CurveHarnessBase.sol";

/// @dev Fees: exact on the curve, fixed per coin at launch, collected from the pool in
///      the right split to the right places no matter who calls, and never charged twice.
contract PoolFeesTest is CurveHarnessBase {
    uint256 internal constant BPS = 10_000;

    /// Curve buy: fee = ceil(pIn x 1.4%) (rounded up, so dust pays too), creator side floor(fee / 2) to the vault, the
    /// rest to the platform, the net into the curve. Exact.
    function testFuzz_curveBuyFeeIsExact(uint256 pIn) public {
        pIn = bound(pIn, 1, 5_000e6);
        _shares(alice, 7_000e6);
        uint256 plat0 = p.balanceOf(platform);
        uint256 vault0 = p.balanceOf(address(vault));
        uint256 q0 = curve.trackedQuote();
        vm.assume(curve.quoteBuy(pIn) > 0);
        _curveBuy(alice, pIn, 0);
        uint256 fee = (pIn * 140 + BPS - 1) / BPS;
        uint256 toCreator = (fee * 5_000) / BPS;
        assertEq(p.balanceOf(address(vault)) - vault0, toCreator);
        assertEq(p.balanceOf(platform) - plat0, fee - toCreator);
        assertEq(curve.trackedQuote() - q0, pIn - fee);
    }

    function testFuzz_curveSellFeeIsExact(uint256 pIn, uint256 frac) public {
        pIn = bound(pIn, 1e6, 5_000e6);
        (uint256 coins,) = _curveBuy(alice, _shares(alice, pIn), 0);
        coins = (coins * bound(frac, 1, 100)) / 100;
        uint256 v = curve.phantom() + curve.trackedQuote();
        uint256 gross = (coins * v) / (curve.trackedTokens() + coins);
        vm.assume(gross > 0);
        uint256 fee = (gross * 140 + BPS - 1) / BPS;
        vm.assume(gross > fee);
        uint256 plat0 = p.balanceOf(platform);
        uint256 vault0 = p.balanceOf(address(vault));
        uint256 out = _curveSell(alice, coins, 0);
        assertEq(out, gross - fee);
        assertEq(p.balanceOf(address(vault)) - vault0, (fee * 5_000) / BPS);
        assertEq(p.balanceOf(platform) - plat0, fee - (fee * 5_000) / BPS);
    }

    /// A coin keeps the fees it launched with, on the curve and in its pool, whatever the
    /// factory is set to later; a later coin gets the new fees.
    function test_feesAreFixedAtLaunch() public {
        vm.prank(owner);
        factory.setFees(Fees(200, 8_000, 15_000, 3_000));
        (Coin c2, BondingCurve cv2) = _launch(ID);
        assertEq(curve.feeBps(), 140);
        assertEq(curve.creatorShareBps(), 5_000);
        assertEq(curve.poolFee(), 10_000);
        assertEq(curve.poolCreatorShareBps(), 5_000);
        assertEq(cv2.feeBps(), 200);
        assertEq(cv2.poolFee(), 15_000);

        // The old coin still charges 1.4% split 50/50.
        uint256 pIn = _shares(alice, 1_000e6);
        uint256 vault0 = p.balanceOf(address(vault));
        _curveBuy(alice, pIn, 0);
        assertEq(p.balanceOf(address(vault)) - vault0, (((pIn * 140 + BPS - 1) / BPS) * 5_000) / BPS);

        // Both graduate with their own pool fee.
        _graduateDirect();
        assertEq(graduator.poolKey(address(coin)).fee, 10_000);
        uint256 pIn2 = _shares(bob, 9_000e6);
        vm.startPrank(bob);
        p.approve(address(cv2), pIn2);
        cv2.buy(pIn2, 0, bob);
        vm.stopPrank();
        assertEq(graduator.poolKey(address(c2)).fee, 15_000);
        (,, address plat2,) = graduator.poolOf(address(c2));
        assertEq(plat2, platform);
    }

    /// The platform address is fixed per coin at launch too: changing it later only
    /// affects new coins, on the curve and in the pool.
    function test_platformIsFixedPerCoin() public {
        address newPlatform = makeAddr("newPlatform");
        uint256 grad = factory.gradUsd();
        vm.prank(owner);
        factory.setConfig(newPlatform, grad);
        uint256 old0 = p.balanceOf(platform);
        _curveBuy(alice, _shares(alice, 500e6), 0);
        assertGt(p.balanceOf(platform), old0);
        assertEq(p.balanceOf(newPlatform), 0);
        _graduateDirect();
        (,, address plat,) = graduator.poolOf(address(coin));
        assertEq(plat, platform);
    }

    /// Pool fees after fuzzed volume in both directions: `collect` by anyone pays the
    /// creator side to the vault and the rest to the platform, in both tokens, exactly,
    /// and leaves the Graduator empty. A second collect finds nothing new.
    function testFuzz_collectSplitsExactlyWhoeverCalls(uint256 volume, uint8 rounds, address caller) public {
        vm.assume(caller != address(0) && caller != address(poolManager));
        _graduateDirect();
        volume = bound(volume, 1e6, 20_000e6);
        rounds = uint8(bound(rounds, 1, 6));
        _shares(alice, (volume * 7) / 10 * rounds + 1e6);
        for (uint256 i; i < rounds; ++i) {
            uint256 pIn = volume / rounds + 1;
            if (pIn > p.balanceOf(alice)) break;
            uint256 coins = _poolSwap(alice, address(p), pIn);
            _poolSwap(alice, address(coin), coins / 2);
        }

        uint256 cp0 = p.balanceOf(caller);
        uint256 cc0 = coin.balanceOf(caller);
        uint256 vp0 = p.balanceOf(address(vault));
        uint256 vc0 = coin.balanceOf(address(vault));
        uint256 pp0 = p.balanceOf(platform);
        uint256 pc0 = coin.balanceOf(platform);
        vm.prank(caller);
        (uint256 fee0, uint256 fee1) = graduator.collect(address(coin));
        (uint256 feeP, uint256 feeC) = address(coin) < address(p) ? (fee1, fee0) : (fee0, fee1);
        assertGt(feeP, 0, "no pToken fees after buys");

        assertEq(p.balanceOf(address(vault)) - vp0, (feeP * 5_000) / BPS);
        assertEq(p.balanceOf(platform) - pp0, feeP - (feeP * 5_000) / BPS);
        assertEq(coin.balanceOf(address(vault)) - vc0, (feeC * 5_000) / BPS);
        assertEq(coin.balanceOf(platform) - pc0, feeC - (feeC * 5_000) / BPS);
        assertEq(p.balanceOf(address(graduator)), 0);
        assertEq(coin.balanceOf(address(graduator)), 0);
        if (caller != platform && caller != address(vault)) {
            assertEq(p.balanceOf(caller), cp0, "caller took pToken fees");
            assertEq(coin.balanceOf(caller), cc0, "caller took coin fees");
        }

        (uint256 again0, uint256 again1) = graduator.collect(address(coin));
        assertEq(again0 + again1, 0, "fees collected twice");
    }

    /// Pool fees are about the pool fee rate of the volume: 1% in, credited on the input side.
    function testFuzz_poolFeeRateMatchesTheCoin(uint256 pIn) public {
        _graduateDirect();
        pIn = bound(pIn, 10e6, 10_000e6);
        uint256 have = _shares(alice, pIn);
        if (pIn > have) pIn = have;
        _poolSwap(alice, address(p), pIn);
        (uint256 fee0, uint256 fee1) = graduator.collect(address(coin));
        uint256 feeP = address(coin) < address(p) ? fee1 : fee0;
        // v4 takes the LP fee on the input amount; allow a unit of rounding.
        assertApproxEqAbs(feeP, (pIn * 10_000) / 1_000_000, 2);
    }

    /// A buy through the Router that crosses graduation pays the curve fee on the curve
    /// part and the pool fee on the pool part, and nothing else: total pToken the buyer
    /// spent equals curve raise delta + fees + the pool's intake.
    function test_crossingBuyIsNotChargedTwice() public {
        _curveBuy(alice, _shares(alice, 2_000e6), 0);
        uint256 q0 = curve.trackedQuote();
        uint256 pm0 = p.balanceOf(address(poolManager));
        uint256 plat0 = p.balanceOf(platform);
        uint256 vault0 = p.balanceOf(address(vault));
        uint256 pOut = exchange.mintOut(px[ID], 12_000e6);
        _routerBuy(bob, 12_000e6, 0);
        assertTrue(curve.graduated());
        uint256 moved = (p.balanceOf(address(poolManager)) - pm0) + (p.balanceOf(platform) - plat0)
            + (p.balanceOf(address(vault)) - vault0);
        assertEq(moved, q0 + pOut, "pToken unaccounted for across graduation");
        _assertRouterEmpty();
        // Pool-side fees exist only for the pool leg and are still in the pool until collected.
        (uint256 f0, uint256 f1) = graduator.collect(address(coin));
        uint256 feeP = address(coin) < address(p) ? f1 : f0;
        assertGt(feeP, 0);
        assertLt(feeP, (pOut * 10_000) / 1_000_000, "pool fee charged on more than the pool leg");
    }

    function test_collectBeforeGraduationReverts() public {
        vm.expectRevert(abi.encodeWithSelector(Graduator.NotGraduated.selector, address(coin)));
        graduator.collect(address(coin));
    }
}
