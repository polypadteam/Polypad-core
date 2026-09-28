// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {Router} from "../../src/Router.sol";

import {ForkBase} from "./ForkBase.t.sol";

/**
 * @dev Gas on live Robinhood Chain state (real USDG, real PoolManager), against
 *      the limits our services send with:
 *        services/api/swap.ts  GAS = { buy: 1.6M, sell: 1.0M, claim: 600k } (the fallback
 *                              when estimation fails, and the limit used when the
 *                              user's tx still needs an approval first)
 *        BondingCurve.GRADUATION_GAS = 1.2M
 *      Run with -vv to print the table.
 */
contract ForkGasTest is ForkBase {
    uint256 internal constant API_BUY = 1_600_000;
    uint256 internal constant API_SELL = 1_000_000;
    uint256 internal constant API_CLAIM = 600_000;

    function _routerBuyGas(address who, BondingCurve c, uint256 usdgIn) internal returns (uint256 used) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(who);
        uint256 g = gasleft();
        router.buy(c, usdgIn, 0, who, q, sig);
        used = g - gasleft();
    }

    /// @dev Smallest gas limit a call succeeds with, as eth_estimateGas finds it.
    function _minGas(address from, address to, bytes memory data, uint256 hi) internal returns (uint256) {
        uint256 lo = 21_000;
        uint256 snap = vm.snapshotState();
        while (hi - lo > 1_000) {
            uint256 mid = (lo + hi) / 2;
            vm.revertToState(snap);
            vm.prank(from);
            (bool ok,) = to.call{gas: mid}(data);
            if (ok) hi = mid;
            else lo = mid;
        }
        vm.revertToState(snap);
        return hi;
    }

    function _graduatingBuyMinGas(uint16 holdersBps) internal returns (uint256 minGas, uint256 used) {
        (, BondingCurve curve) = _launch(ID, holdersBps);
        _buy(alice, curve, 2_000e6);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        bytes memory data = abi.encodeCall(Router.buy, (curve, 6_000e6, 0, bob, q, sig));
        minGas = _minGas(bob, address(router), data, 10_000_000);
        vm.prank(bob);
        uint256 g = gasleft();
        (bool ok,) = address(router).call(data);
        used = g - gasleft();
        assertTrue(ok && curve.graduated());
    }

    function test_gasTradesAgainstApiLimits() public {
        (, BondingCurve plain) = _launch(ID, 0);
        uint256 first = _routerBuyGas(alice, plain, 100e6);
        uint256 next = _routerBuyGas(alice, plain, 100e6);
        uint256 coins = plain.coin().balanceOf(alice);
        uint256 g = gasleft();
        _sell(alice, plain, coins / 2);
        uint256 sell = g - gasleft();
        console2.log("plain  first buy", first);
        console2.log("plain  buy      ", next);
        console2.log("plain  sell     ", sell);

        (, BondingCurve holder) = _launch(ID_B, 5_000);
        vm.prank(owner);
        exchange.setMaxRisk(ID_B, type(uint256).max);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID_B, BUY);
        vm.prank(alice);
        g = gasleft();
        router.buy(holder, 100e6, 0, alice, q, sig);
        uint256 hFirst = g - gasleft();
        console2.log("holder first buy", hFirst);

        assertLt(first * 3 / 2 + 50_000, API_BUY, "estimate x1.5 + 50k over the API buy fallback");
        assertLt(hFirst * 3 / 2 + 50_000, API_BUY);
        assertLt(sell * 3 / 2 + 50_000, API_SELL);
    }

    /// @dev The graduating buy must fit the API's fixed 1.6M (used when an approval
    ///      is still pending, so the estimate is unavailable) and the curve's floor.
    function test_gasGraduatingBuyPlain() public {
        (uint256 minGas, uint256 used) = _graduatingBuyMinGas(0);
        console2.log("plain  graduating buy: min gas limit", minGas);
        console2.log("plain  graduating buy: gas used     ", used);
        assertLe(minGas, API_BUY, "graduating buy needs more than the API's fixed buy limit");
    }

    function test_gasGraduatingBuyHolderCoin() public {
        (uint256 minGas, uint256 used) = _graduatingBuyMinGas(10_000);
        console2.log("holder graduating buy: min gas limit", minGas);
        console2.log("holder graduating buy: gas used     ", used);
        assertLe(minGas, API_BUY, "graduating buy needs more than the API's fixed buy limit");
    }

    /// @dev What graduation itself costs, to size GRADUATION_GAS against.
    function test_gasGraduationAlone() public {
        (, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        // Retry path: buy right up to the reserve with the graduation failing for lack of
        // nothing... instead measure buy-that-graduates minus a same-size non-graduating buy.
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(bob);
        usdg.approve(address(exchange), type(uint256).max);
        uint256 pIn = exchange.mint(address(curve.pToken()), 6_000e6, 0, bob, q, sig);
        curve.pToken().approve(address(curve), pIn);
        uint256 g = gasleft();
        curve.buy(pIn, 0, bob);
        uint256 grad = g - gasleft();
        vm.stopPrank();
        assertTrue(curve.graduated());
        console2.log("holder curve.buy that graduates (direct)", grad);
        assertLt(grad, curve.GRADUATION_GAS(), "graduation costs more than the floor it demands");
    }

    function test_gasClaimFor200Holders() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        address[] memory hs = new address[](200);
        for (uint256 i; i < 200; ++i) {
            hs[i] = address(uint160(0x10000 + i));
            _fund(hs[i], 20e6);
            vm.prank(hs[i]);
            usdg.approve(address(router), type(uint256).max);
            _buy(hs[i], curve, 20e6);
        }
        vm.warp(block.timestamp + 1 hours + 1);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.prank(keeper);
        uint256 g = gasleft();
        uint256 paid = vault.claimFor(address(c), hs, 0, false, q, sig);
        uint256 used = g - gasleft();
        assertGt(paid, 0);
        console2.log("claimFor 200 holders", used);
        console2.log("  per holder        ", used / 200);
        assertLt(used * 13 / 10 + 50_000, 32_000_000, "keeper limit over the per-tx cap");

        // A single self-claim as USDG, against the API's claim limit.
        _buy(alice, curve, 100e6);
        vm.warp(block.timestamp + 1 hours + 1);
        address[] memory me = new address[](1);
        me[0] = alice;
        (q, sig) = signedQuote(ID, SELL);
        vm.prank(alice);
        g = gasleft();
        vault.claimFor(address(c), me, 0, true, q, sig);
        uint256 one = g - gasleft();
        console2.log("claimFor self as USDG", one);
        assertLt(one * 3 / 2 + 50_000, API_CLAIM);
    }

    function test_gasPayQueue100() public {
        (, BondingCurve curve) = _launch(ID, 0);
        address[] memory who = new address[](100);
        uint256[] memory coins = new uint256[](100);
        for (uint256 i; i < 100; ++i) {
            who[i] = address(uint160(0x20000 + i));
            _fund(who[i], 30e6);
            vm.prank(who[i]);
            usdg.approve(address(router), type(uint256).max);
            (coins[i],) = _buy(who[i], curve, 30e6);
        }
        uint256 all = usdg.balanceOf(address(exchange));
        vm.prank(keeper);
        exchange.sendToBridge(all);
        for (uint256 i; i < 100; ++i) _sell(who[i], curve, coins[i]);
        assertEq(exchange.queueLength(), 100);
        _fund(address(exchange), exchange.queued());
        uint256 g = gasleft();
        exchange.payQueue(100);
        uint256 used = g - gasleft();
        assertEq(exchange.queueLength(), 0);
        console2.log("payQueue(100)", used);
    }
}
