// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../src/BondingCurve.sol";
import {Coin} from "../src/Coin.sol";
import {PExchange} from "../src/PExchange.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {PToken} from "../src/PToken.sol";
import {Router} from "../src/Router.sol";
import {Vm} from "forge-std/Vm.sol";

import {PolypadBase} from "./Polypad.t.sol";

/// @dev The posted-price path, the circuit breaker and the Router's Swap event.
contract PostedTest is PolypadBase {
    address internal poster = makeAddr("poster");
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;

    event Swap(
        address indexed coin,
        address indexed trader,
        address indexed curve,
        bool isBuy,
        uint256 usdg,
        uint256 coins,
        uint256 pTokens,
        uint256 pTokenRefund,
        bool posted
    );

    function setUp() public override {
        super.setUp();
        (coin, curve) = _launch(ID);
        p = exchange.pTokenOf(ID);
        vm.startPrank(owner);
        oracle.setPoster(poster);
        exchange.setMaxUnbacked(ID, type(uint256).max);
        vm.stopPrank();
        _postOnChain(ID, 600_000);
    }

    function _postOnChain(uint256 id, uint64 price) internal {
        uint256[] memory ids = new uint256[](1);
        uint64[] memory prices = new uint64[](1);
        ids[0] = id;
        prices[0] = price;
        vm.prank(poster);
        oracle.post(ids, prices);
    }

    function _buyPosted(address who, uint256 usdgIn) internal returns (uint256 coins) {
        vm.prank(who);
        (coins,) = router.buyPosted(curve, usdgIn, 0, who);
    }

    function _sellPosted(address who, uint256 coins) internal returns (uint256 out) {
        vm.startPrank(who);
        coin.approve(address(router), coins);
        out = router.sellPosted(curve, coins, 0, who);
        vm.stopPrank();
    }

    /* ------------------------------------------------------------ posted path */

    function test_postedBuyPaysPostedPricePlusWiderSpread() public {
        _buyPosted(alice, 300e6);
        // 300 USDG at 60c + 1.5% = 60.9c buys 492.61 shares.
        uint256 price = (uint256(600_000) * 10_150 + 9_999) / 10_000;
        assertEq(p.totalSupply(), (uint256(300e6) * 1e6) / price);
    }

    function test_postedSellGetsPostedPriceLessWiderSpread() public {
        uint256 coins = _buyPosted(alice, 300e6);
        uint256 before = usdg.balanceOf(alice);
        uint256 out = _sellPosted(alice, coins);
        assertEq(usdg.balanceOf(alice) - before, out);
        // Round trip costs both posted spreads (1.5%) and the curve fees (1.4%), about 6%.
        assertLt(out, 300e6);
        assertGt(out, 280e6);
    }

    function _alive() internal {
        vm.prank(poster);
        oracle.alive();
    }

    function test_aQuietMarketStaysTradableWhileThePosterIsAlive() public {
        // No new post for 30 minutes, but the poster keeps checking in.
        for (uint256 i; i < 60; ++i) {
            vm.warp(block.timestamp + 30);
            _alive();
        }
        _buyPosted(alice, 10e6);
    }

    function test_aSilentPosterClosesThePostedPath() public {
        vm.warp(block.timestamp + oracle.maxPosterSilence() + 1);
        uint64 aliveAt = oracle.posterAliveAt();
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PosterSilent.selector, aliveAt));
        router.buyPosted(curve, 10e6, 0, alice);
        // The signed path does not depend on the poster.
        _buy(alice, curve, 10e6);

        _alive();
        _buyPosted(alice, 10e6);
    }

    function test_aPostOlderThanMaxAgeIsRefusedEvenWhileAlive() public {
        for (uint256 t; t <= oracle.maxPostAge(); t += 60) {
            vm.warp(block.timestamp + 60);
            _alive();
        }
        vm.prank(alice);
        vm.expectRevert();
        router.buyPosted(curve, 10e6, 0, alice);
        _postOnChain(ID, 600_000);
        _buyPosted(alice, 10e6);
    }

    function test_marketNeverPostedCannotTradePosted() public {
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.NoPostedPrice.selector, ID_B));
        oracle.postedPrice(ID_B, BUY);
    }

    function test_aJumpHaltsThePostedPathForTheCooldown() public {
        _postOnChain(ID, 610_000); // 1c: no halt
        _buyPosted(alice, 10e6);

        _postOnChain(ID, 400_000); // 21c jump: halted
        vm.prank(alice);
        vm.expectRevert();
        router.buyPosted(curve, 10e6, 0, alice);
        // The signed path is untouched by a posted halt.
        _buy(alice, curve, 10e6);

        vm.warp(block.timestamp + oracle.postCooldown());
        _alive();
        _buyPosted(alice, 10e6);
    }

    function test_pausedMarketBlocksPostedBuysNotSells() public {
        uint256 coins = _buyPosted(alice, 50e6);
        _pause(ID);
        vm.prank(alice);
        vm.expectRevert();
        router.buyPosted(curve, 10e6, 0, alice);
        assertGt(_sellPosted(alice, coins), 0);
    }

    function test_postedTradesAreCappedPerTradeAndPerBlock() public {
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PostedTradeTooLarge.selector, 501e6, 500e6));
        router.buyPosted(curve, 501e6, 0, alice);

        _buyPosted(alice, 500e6);
        _buyPosted(bob, 500e6);
        _buyPosted(alice, 500e6);
        _buyPosted(bob, 500e6);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PostedBlockCap.selector, 2_001e6, 2_000e6));
        router.buyPosted(curve, 1e6, 0, alice);

        vm.roll(block.number + 1);
        _buyPosted(alice, 1e6);
    }

    function test_settledMarketPaysPayoutOnThePostedPath() public {
        uint256 coins = _buyPosted(alice, 100e6);
        vm.prank(keeper);
        oracle.settle(ID, 1e6);
        // Settled: no post needed, no posted caps, the payout applies.
        vm.warp(block.timestamp + 1 days);
        assertGt(_sellPosted(alice, coins), 0);
    }

    function test_onlyThePosterPostsAndPricesStayInRange() public {
        uint256[] memory ids = new uint256[](1);
        uint64[] memory prices = new uint64[](1);
        ids[0] = ID;
        prices[0] = 500_000;
        vm.prank(alice);
        vm.expectRevert(PriceOracle.OnlyPoster.selector);
        oracle.post(ids, prices);

        prices[0] = 1e6;
        vm.prank(poster);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID, uint64(1e6)));
        oracle.post(ids, prices);
    }

    /* -------------------------------------------------------- circuit breaker */

    function test_outflowIsCappedPerHourOnBothPaths() public {
        vm.prank(owner);
        exchange.setOutflowCap(100e6);
        (uint256 coins,) = _buy(alice, curve, 1_000e6);

        // About 66 USDG out: under the cap.
        uint256 out1 = _sell(alice, curve, coins / 20);
        assertGt(out1, 0);
        assertLt(out1, 100e6);
        // Another as large would pass 100 USDG this hour.
        vm.startPrank(alice);
        coin.approve(address(router), coins / 20);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.expectRevert();
        router.sell(curve, coins / 20, 0, alice, q, sig);
        vm.stopPrank();

        // Next hour: room again.
        vm.warp((block.timestamp / 3_600 + 1) * 3_600);
        assertGt(_sell(alice, curve, coins / 20), 0);
    }

    function test_keeperHaltsEverythingOnlyOwnerResumes() public {
        (uint256 coins,) = _buy(alice, curve, 100e6);

        vm.prank(alice);
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.halt();

        vm.prank(keeper);
        exchange.halt();
        vm.prank(bob);
        vm.expectRevert(PExchange.Halted.selector);
        router.buyPosted(curve, 10e6, 0, bob);
        vm.startPrank(alice);
        coin.approve(address(router), coins);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.expectRevert(PExchange.Halted.selector);
        router.sell(curve, coins, 0, alice, q, sig);
        vm.stopPrank();

        vm.prank(keeper);
        vm.expectRevert();
        exchange.resume();
        vm.prank(owner);
        exchange.resume();
        assertGt(_sell(alice, curve, coins), 0);
    }

    function test_quotesCannotBeValidForLongerThan30s() public {
        PriceOracle.Quote memory q =
            PriceOracle.Quote(ID, BUY, 600_000, type(uint256).max, uint64(block.timestamp + 31));
        bytes memory sig = _sign(q, signerPk);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteTooLong.selector, q.validUntil));
        router.buy(curve, 10e6, 0, alice, q, sig);
    }

    /* ------------------------------------------------------------ Swap event */

    function test_everyTradeEmitsOneSwapWithUsdg() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        uint256 pOut = exchange.mintOut(q.price, 60e6);
        uint256 coinsOut = curve.quoteBuy(pOut);
        vm.expectEmit(true, true, true, true, address(router));
        emit Swap(address(coin), alice, address(curve), true, 60e6, coinsOut, pOut, 0, false);
        vm.prank(alice);
        router.buy(curve, 60e6, 0, alice, q, sig);

        vm.recordLogs();
        uint256 out = _sellPosted(alice, coinsOut);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 topic = keccak256("Swap(address,address,address,bool,uint256,uint256,uint256,uint256,bool)");
        uint256 swaps;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != address(router) || logs[i].topics[0] != topic) continue;
            ++swaps;
            (bool isBuy, uint256 usdgOut,,,, bool posted) =
                abi.decode(logs[i].data, (bool, uint256, uint256, uint256, uint256, bool));
            assertFalse(isBuy);
            assertEq(usdgOut, out);
            assertTrue(posted);
        }
        assertEq(swaps, 1);
    }
}
