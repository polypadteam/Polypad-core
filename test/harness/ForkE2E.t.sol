// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";

import {ForkBase} from "./ForkBase.t.sol";

/// @dev The whole product on real USDG and the live v4 PoolManager.
contract ForkE2ETest is ForkBase {
    function _pOf(Coin c) internal view returns (PToken) {
        (,, address p,,) = vault.launches(address(c));
        return PToken(p);
    }

    function test_signedMintRedeemAndDelayedSaleOnRealUsdg() public {
        (Coin c,) = _launch(ID, 0);
        PToken p = _pOf(c);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), type(uint256).max);
        uint256 before = usdg.balanceOf(alice);
        uint256 shares = exchange.mint(address(p), 600e6, 0, alice, q, sig);
        assertEq(before - usdg.balanceOf(alice), 600e6);
        assertEq(shares, exchange.mintOut(600_000, 600e6));
        vm.stopPrank();

        (q, sig) = signedQuote(ID, SELL);
        vm.prank(alice);
        uint256 out = exchange.redeem(address(p), shares / 2, 0, alice, q, sig);
        assertEq(out, exchange.redeemOut(600_000, shares / 2));
        assertEq(usdg.balanceOf(alice), before - 600e6 + out);

        // Over the hourly cap: the sale waits DELAY, then anyone releases it.
        vm.prank(owner);
        exchange.setOutflowCap(0, 0);
        uint256 rest = p.balanceOf(alice);
        vm.prank(alice);
        uint256 owed = exchange.redeem(address(p), rest, 0, alice, q, sig);
        assertEq(usdg.balanceOf(alice), before - 600e6 + out, "nothing paid yet");
        assertEq(exchange.delayedShares(ID), rest);
        vm.warp(block.timestamp + exchange.DELAY());
        exchange.release(0);
        assertEq(usdg.balanceOf(alice), before - 600e6 + out + owed);
        assertEq(exchange.delayedShares(ID), 0);
        assertEq(p.balanceOf(address(exchange)), 0);
    }

    Coin internal gc;
    BondingCurve internal gcurve;
    PToken internal gp;

    function test_launchToGraduationPoolTradesFeesAndDividends() public {
        (gc, gcurve) = _launch(ID, 5_000);
        gp = _pOf(gc);

        // Curve phase, several holders.
        _buy(alice, gcurve, 1_500e6);
        _buy(bob, gcurve, 800e6);
        uint256 quarter = gc.balanceOf(alice) / 4;
        vm.prank(alice);
        gc.transfer(carol, quarter);

        // Sell out: graduates into a real v4 pool, the rest spent in it.
        (uint256 coins,) = _buy(bob, gcurve, 7_000e6);
        assertTrue(gcurve.graduated());
        assertTrue(graduator.graduated(address(gc)));
        assertGt(coins, 0);

        _poolRoundTrips();
        _collectPoolFees();
        _payDividends();
        _creatorCashesOut();
    }

    function _poolRoundTrips() internal {
        for (uint256 i; i < 4; ++i) {
            uint256 usdgIn = 250e6 * (i + 1);
            uint256 quoted = router.quotePool(gcurve, true, exchange.mintOut(px[ID], usdgIn));
            (uint256 got,) = _buy(carol, gcurve, usdgIn);
            assertEq(got, quoted, "pool buy quote");
            uint256 qSell = router.quotePool(gcurve, false, got / 2);
            uint256 before = usdg.balanceOf(carol);
            uint256 out = _sell(carol, gcurve, got / 2);
            assertEq(out, exchange.redeemOut(px[ID], qSell), "pool sell quote");
            assertEq(usdg.balanceOf(carol) - before, out);
        }
    }

    function _collectPoolFees() internal {
        uint256 credBefore = vault.owed(address(gp), creator);
        uint256 platBefore = gp.balanceOf(platform);
        graduator.collect(address(gc));
        assertGt(vault.owed(address(gp), creator), credBefore);
        assertGt(gp.balanceOf(platform), platBefore);
    }

    function _payDividends() internal {
        vm.warp(block.timestamp + 1 hours + 1);
        address[] memory hs = new address[](4);
        hs[0] = alice;
        hs[1] = bob;
        hs[2] = carol;
        hs[3] = address(poolManager); // excluded: owed nothing, skipped
        uint256 total;
        for (uint256 i; i < 3; ++i) total += vault.pending(address(gc), hs[i]);
        uint256 aliceOwed = vault.pending(address(gc), alice);
        assertEq(vault.pending(address(gc), address(poolManager)), 0, "the pool earns nothing");
        assertGt(aliceOwed, 0);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 aliceP = gp.balanceOf(alice);
        uint256 aliceUsd = usdg.balanceOf(alice);
        vm.prank(keeper);
        uint256 paid = vault.claimFor(address(gc), hs, 0, true, q, sig); // cashOut ignored for others
        assertEq(paid, total);
        assertEq(gp.balanceOf(alice) - aliceP, aliceOwed);
        assertEq(usdg.balanceOf(alice), aliceUsd, "nobody sold alice's rewards");
    }

    function _creatorCashesOut() internal {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 owed = vault.owed(address(gp), creator);
        uint256 cBefore = usdg.balanceOf(creator);
        vm.prank(creator);
        uint256 usd = vault.withdrawUsd(address(gp), creator, 0, q, sig);
        assertEq(usdg.balanceOf(creator) - cBefore, usd);
        assertEq(usd, exchange.redeemOut(px[ID], owed));
        (,, uint256 pot, uint256 streaming,,) = vault.books(address(gc));
        assertGe(gp.balanceOf(address(vault)), pot + streaming + vault.owed(address(gp), creator), "vault short");
    }

    /// @dev Pool quotes are exact for any size, both ways, on the real PoolManager.
    function testFuzz_poolQuoteEqualsExecution(uint256 usdgIn, uint256 sellFrac) public {
        (Coin c, BondingCurve curve) = _launch(ID, 0);
        _buy(bob, curve, 7_000e6);
        assertTrue(curve.graduated());
        usdgIn = bound(usdgIn, 1e6, 50_000e6);
        sellFrac = bound(sellFrac, 1, 100);

        uint256 pIn = exchange.mintOut(px[ID], usdgIn);
        uint256 quoted = router.quotePool(curve, true, pIn);
        _fund(alice, usdgIn);
        (uint256 got,) = _buy(alice, curve, usdgIn);
        assertEq(got, quoted);

        uint256 sellCoins = (got * sellFrac) / 100;
        if (sellCoins == 0) return;
        uint256 qOut = router.quotePool(curve, false, sellCoins);
        uint256 out = _sell(alice, curve, sellCoins);
        assertEq(out, exchange.redeemOut(px[ID], qOut));
        assertEq(c.balanceOf(alice), got - sellCoins);
    }

    /// @dev Price impact grows with size: a bigger buy never gets a better average price.
    function testFuzz_poolPriceImpactIsMonotone(uint256 a, uint256 b) public {
        (, BondingCurve curve) = _launch(ID, 0);
        _buy(bob, curve, 7_000e6);
        a = bound(a, 1e6, 20_000e6);
        b = bound(b, a + 1e6, 40_000e6);
        uint256 outA = router.quotePool(curve, true, a);
        uint256 outB = router.quotePool(curve, true, b);
        assertGt(outB, outA);
        // coins per pToken: B's average is no better than A's
        assertLe(outB * a, outA * b + b);
    }
}
