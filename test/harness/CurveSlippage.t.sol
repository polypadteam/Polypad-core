// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../../src/BondingCurve.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {Router} from "../../src/Router.sol";

import {CurveHarnessBase} from "./CurveHarnessBase.sol";

/// @dev Every entry point honours its minimum output exactly: `== expected` passes,
///      `expected + 1` reverts, before graduation, after it, and across it.
contract CurveSlippageTest is CurveHarnessBase {
    /* ------------------------------------------------------------ curve direct */

    function testFuzz_curveBuyMinOutBoundary(uint256 usdgIn) public {
        usdgIn = bound(usdgIn, 1e6, 5_000e6); // below the sell-out
        uint256 pIn = _shares(alice, usdgIn);
        uint256 expected = curve.quoteBuy(pIn);

        vm.startPrank(alice);
        p.approve(address(curve), pIn);
        vm.expectRevert(abi.encodeWithSelector(BondingCurve.Slippage.selector, expected, expected + 1));
        curve.buy(pIn, expected + 1, alice);
        (uint256 out,) = curve.buy(pIn, expected, alice);
        vm.stopPrank();
        assertEq(out, expected);
    }

    function testFuzz_curveSellMinOutBoundary(uint256 usdgIn, uint256 frac) public {
        usdgIn = bound(usdgIn, 1e6, 5_000e6);
        (uint256 coins,) = _curveBuy(alice, _shares(alice, usdgIn), 0);
        coins = (coins * bound(frac, 1, 100)) / 100;
        uint256 expected = curve.quoteSell(coins);

        vm.startPrank(alice);
        coin.approve(address(curve), coins);
        vm.expectRevert(abi.encodeWithSelector(BondingCurve.Slippage.selector, expected, expected + 1));
        curve.sell(coins, expected + 1, alice);
        uint256 out = curve.sell(coins, expected, alice);
        vm.stopPrank();
        assertEq(out, expected);
    }

    /// The sell-out buy reports exactly the sellable remainder; minOut above it reverts.
    function test_curveSellOutBuyMinOutIsTheSellableRemainder() public {
        _curveBuy(alice, _shares(alice, 3_000e6), 0);
        uint256 sellable = curve.trackedTokens() - curve.reserved();
        uint256 pIn = _shares(bob, 8_000e6);
        vm.startPrank(bob);
        p.approve(address(curve), pIn);
        vm.expectRevert(abi.encodeWithSelector(BondingCurve.Slippage.selector, sellable, sellable + 1));
        curve.buy(pIn, sellable + 1, bob);
        (uint256 out,) = curve.buy(pIn, sellable, bob);
        vm.stopPrank();
        assertEq(out, sellable);
        assertTrue(curve.graduated());
    }

    /* ------------------------------------------------------------ router, signed */

    function testFuzz_routerBuyMinCoinsBoundaryOnCurve(uint256 usdgIn) public {
        usdgIn = bound(usdgIn, 1e6, 5_000e6);
        uint256 expected = curve.quoteBuy(exchange.mintOut(px[ID], usdgIn));
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(Router.Slippage.selector, expected, expected + 1));
        router.buy(curve, usdgIn, expected + 1, alice, q, sig);
        (uint256 coins,) = _routerBuy(alice, usdgIn, expected);
        assertEq(coins, expected);
        _assertRouterEmpty();
    }

    function testFuzz_routerSellMinUsdgBoundaryOnCurve(uint256 usdgIn, uint256 frac) public {
        usdgIn = bound(usdgIn, 2e6, 5_000e6);
        (uint256 coins,) = _routerBuy(alice, usdgIn, 0);
        coins = (coins * bound(frac, 1, 100)) / 100;
        uint256 expected = exchange.redeemOut(px[ID], curve.quoteSell(coins));
        vm.assume(expected > 0);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.startPrank(alice);
        coin.approve(address(router), coins);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, expected, expected + 1));
        router.sell(curve, coins, expected + 1, alice, q, sig);
        uint256 out = router.sell(curve, coins, expected, alice, q, sig);
        vm.stopPrank();
        assertEq(out, expected);
        _assertRouterEmpty();
    }

    /* ------------------------------------------------------------ router, posted */

    function testFuzz_buyPostedMinCoinsBoundary(uint256 usdgIn) public {
        usdgIn = bound(usdgIn, 1e6, exchange.postedMaxTrade());
        uint256 expected = curve.quoteBuy(exchange.mintPostedOut(ID, usdgIn));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(Router.Slippage.selector, expected, expected + 1));
        router.buyPosted(curve, usdgIn, expected + 1, alice);
        vm.prank(alice);
        (uint256 coins,) = router.buyPosted(curve, usdgIn, expected, alice);
        assertEq(coins, expected);
        _assertRouterEmpty();
    }

    function testFuzz_sellPostedMinUsdgBoundary(uint256 usdgIn, uint256 frac) public {
        usdgIn = bound(usdgIn, 2e6, exchange.postedMaxTrade());
        (uint256 coins,) = _routerBuy(alice, usdgIn, 0);
        coins = (coins * bound(frac, 1, 100)) / 100;
        uint256 expected = exchange.redeemPostedOut(ID, curve.quoteSell(coins));
        vm.assume(expected > 0);
        vm.startPrank(alice);
        coin.approve(address(router), coins);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, expected, expected + 1));
        router.sellPosted(curve, coins, expected + 1, alice);
        uint256 out = router.sellPosted(curve, coins, expected, alice);
        vm.stopPrank();
        assertEq(out, expected);
        _assertRouterEmpty();
    }

    /* ------------------------------------------------------------ pool path */

    function testFuzz_routerBuyMinCoinsBoundaryInPool(uint256 usdgIn) public {
        _graduateDirect();
        usdgIn = bound(usdgIn, 1e6, 50_000e6);
        uint256 expected = router.quotePool(curve, true, exchange.mintOut(px[ID], usdgIn));
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(Router.Slippage.selector, expected, expected + 1));
        router.buy(curve, usdgIn, expected + 1, alice, q, sig);
        (uint256 coins,) = _routerBuy(alice, usdgIn, expected);
        assertEq(coins, expected);
        _assertRouterEmpty();
    }

    function testFuzz_routerSellMinUsdgBoundaryInPool(uint256 usdgIn, uint256 frac) public {
        _graduateDirect();
        usdgIn = bound(usdgIn, 2e6, 20_000e6);
        (uint256 coins,) = _routerBuy(alice, usdgIn, 0);
        coins = (coins * bound(frac, 1, 100)) / 100;
        uint256 expected = exchange.redeemOut(px[ID], router.quotePool(curve, false, coins));
        vm.assume(expected > 0);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.startPrank(alice);
        coin.approve(address(router), coins);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, expected, expected + 1));
        router.sell(curve, coins, expected + 1, alice, q, sig);
        uint256 out = router.sell(curve, coins, expected, alice, q, sig);
        vm.stopPrank();
        assertEq(out, expected);
        _assertRouterEmpty();
    }

    /* ------------------------------------------------------------ across graduation */

    /// A buy that sells the curve out and spends the rest in the new pool: minCoins
    /// covers both legs together.
    function testFuzz_routerBuyAcrossGraduationMinCoinsBoundary(uint256 pre, uint256 usdgIn) public {
        pre = bound(pre, 0, 5_000e6);
        // Below ~70 units the rounded-up fee takes the whole buy (ZeroAmount): skip.
        if (pre >= 100) _routerBuy(bob, pre, 0);
        usdgIn = bound(usdgIn, 8_000e6, 30_000e6); // always crosses

        uint256 snap = vm.snapshotState();
        (uint256 expected, uint256 refund) = _routerBuy(alice, usdgIn, 0);
        assertTrue(curve.graduated());
        assertEq(refund, 0, "rest spent in the pool");
        vm.revertToState(snap);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(Router.Slippage.selector, expected, expected + 1));
        router.buy(curve, usdgIn, expected + 1, alice, q, sig);
        assertFalse(curve.graduated(), "revert undoes the graduation too");
        (uint256 coins,) = _routerBuy(alice, usdgIn, expected);
        assertEq(coins, expected);
        assertEq(coin.balanceOf(alice), coins);
        _assertRouterEmpty();
    }

    /// Same across graduation on the posted path (two buys fit under the per-trade cap only
    /// if the curve is nearly sold out first).
    function test_buyPostedAcrossGraduation() public {
        // Leave less than one posted trade's worth on the curve.
        uint256 target = curve.target();
        uint256 pIn = _shares(bob, 7_000e6);
        // Buy almost all: find a pIn leaving ~100 shares of raise.
        uint256 need = target - curve.trackedQuote();
        uint256 gross = ((need - 150e6) * 10_000) / (10_000 - curve.feeBps());
        _curveBuy(bob, gross, 0);
        assertFalse(curve.graduated());
        pIn; // silence

        uint256 snap = vm.snapshotState();
        vm.prank(alice);
        (uint256 expected,) = router.buyPosted(curve, 500e6, 0, alice);
        assertTrue(curve.graduated());
        vm.revertToState(snap);

        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(Router.Slippage.selector, expected, expected + 1));
        router.buyPosted(curve, 500e6, expected + 1, alice);
        vm.prank(alice);
        (uint256 coins,) = router.buyPosted(curve, 500e6, expected, alice);
        assertEq(coins, expected);
        _assertRouterEmpty();
    }
}
