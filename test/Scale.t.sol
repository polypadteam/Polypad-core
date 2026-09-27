// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IHooks} from "v4-core/src/interfaces/IHooks.sol";
import {Currency} from "v4-core/src/types/Currency.sol";
import {PoolKey} from "v4-core/src/types/PoolKey.sol";

import {BondingCurve} from "../src/BondingCurve.sol";
import {Coin} from "../src/Coin.sol";
import {Graduator} from "../src/Graduator.sol";
import {PExchange} from "../src/PExchange.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {PToken} from "../src/PToken.sol";

import {PolypadBase} from "./Polypad.t.sol";

/// @dev Graduation into Uniswap v4, the redemption queue, trading after resolution,
///      and bursts of trades far larger than a single float refill.
contract ScaleTest is PolypadBase {
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;

    function setUp() public override {
        super.setUp();
        (coin, curve) = _launch(ID);
        p = exchange.pTokenOf(ID);
        vm.prank(owner);
        exchange.setMaxUnbacked(ID, type(uint256).max);
    }

    function _graduate() internal returns (uint256 coins) {
        (coins,) = _buy(alice, curve, 7_000e6);
        assertTrue(curve.graduated());
    }

    /* ---------------------------------------------------------- graduation */

    function test_graduationKeepsThePriceAndEmptiesTheCurve() public {
        // The curve's price at its last coin: (phantom + target) / reserved.
        uint256 finalPrice = ((curve.phantom() + curve.target()) * 1e18) / curve.reserved();

        // Buy on the curve directly with pToken, so the excess is refunded, not spent in the pool.
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), 7_000e6);
        uint256 pIn = exchange.mint(address(p), 7_000e6, 0, alice, q, sig);
        p.approve(address(curve), pIn);
        (, uint256 refund) = curve.buy(pIn, 0, alice);
        vm.stopPrank();
        assertGt(refund, 0);

        assertTrue(curve.graduated());
        assertEq(curve.trackedQuote(), 0);
        assertEq(coin.balanceOf(address(curve)), 0);
        assertEq(p.balanceOf(address(curve)), 0);
        // The pool opens at the curve's final price (spotPrice has only ~2 significant digits here).
        assertApproxEqAbs(curve.spotPrice(), finalPrice, 1);

        // Unused reserve was burned, the rest is in the pool.
        uint256 dead = coin.balanceOf(curve.DEAD());
        assertGt(dead, 0);
        assertLt(dead, curve.reserved());
    }

    function test_buyAndSellInThePoolThroughTheRouter() public {
        _graduate();
        uint256 quoted = router.quotePool(curve, true, exchange.mintOut(px[ID], 500e6));
        (uint256 coins,) = _buy(bob, curve, 500e6);
        assertEq(coins, quoted);

        uint256 before = usdg.balanceOf(bob);
        uint256 out = _sell(bob, curve, coins);
        assertEq(usdg.balanceOf(bob) - before, out);
        // Round trip: two 1% pool fees, two 0.25% spreads, a little price impact.
        assertApproxEqRel(out, 500e6 * 975 / 1000, 0.01e18);
    }

    function test_poolFeesSplit70To30() public {
        _graduate();
        for (uint256 i; i < 5; ++i) {
            (uint256 c,) = _buy(bob, curve, 1_000e6);
            _sell(bob, curve, c);
        }
        uint256 creatorBefore = vault.owed(address(p), creator);
        uint256 platformBefore = p.balanceOf(platform);
        graduator.collect(address(coin));
        uint256 toCreator = vault.owed(address(p), creator) - creatorBefore;
        uint256 toPlatform = p.balanceOf(platform) - platformBefore;
        // ~$5k of pToken bought through the pool: ~1% of it in pToken fees.
        assertGt(toCreator, 0);
        assertApproxEqRel(toCreator * 3, toPlatform * 7, 0.001e18);
        assertGt(vault.owed(address(coin), creator), 0); // sell-side fees come in coins
    }

    function test_nobodyElseCanCreateAPolypadPool() public {
        bool coinIs0 = address(coin) < address(p);
        PoolKey memory key = PoolKey({
            currency0: Currency.wrap(coinIs0 ? address(coin) : address(p)),
            currency1: Currency.wrap(coinIs0 ? address(p) : address(coin)),
            fee: 10_000,
            tickSpacing: 200,
            hooks: IHooks(address(graduator))
        });
        vm.expectRevert();
        poolManager.initialize(key, 1 << 96);
    }

    function test_onlyFactoryCurvesGraduate() public {
        vm.expectRevert(Graduator.OnlyCurve.selector);
        graduator.graduate(coin, p, 1, 1, alice, alice);
    }

    function test_curveRefusesDirectTradesAfterGraduation() public {
        _graduate();
        vm.expectRevert(BondingCurve.Graduated_.selector);
        curve.buy(1e6, 0, alice);
        vm.expectRevert(BondingCurve.Graduated_.selector);
        curve.sell(1e18, 0, alice);
    }

    /* -------------------------------------------------------- redemption queue */

    function test_shortFloatQueuesAndPaysInOrder() public {
        (uint256 a,) = _buy(alice, curve, 1_000e6);
        (uint256 b,) = _buy(bob, curve, 1_000e6);
        // The float went to the desk.
        uint256 all = usdg.balanceOf(address(exchange));
        vm.prank(keeper);
        exchange.sendToBridge(all);

        uint256 aliceBefore = usdg.balanceOf(alice);
        uint256 owedA = _sell(alice, curve, a);
        uint256 owedB = _sell(bob, curve, b);
        assertEq(usdg.balanceOf(alice), aliceBefore); // not paid yet, but the sell went through
        assertEq(exchange.queueLength(), 2);
        assertEq(exchange.queued(), owedA + owedB);

        // Keeper cannot send owed float away.
        usdg.mint(address(exchange), owedA);
        vm.prank(keeper);
        vm.expectRevert();
        exchange.sendToBridge(1);

        // Bridge lands: anyone pays the queue, oldest first.
        exchange.payQueue(10);
        assertEq(usdg.balanceOf(alice), aliceBefore + owedA);
        assertEq(exchange.queueLength(), 1);
        usdg.mint(address(exchange), owedB);
        exchange.payQueue(10);
        assertEq(exchange.queueLength(), 0);
        assertEq(exchange.queued(), 0);
    }

    function test_aShortFloatPaysWhatItHasAndQueuesTheRest() public {
        (uint256 a,) = _buy(alice, curve, 1_000e6);
        // Leave $100 of free float.
        uint256 free = exchange.freeFloat();
        vm.prank(keeper);
        exchange.sendToBridge(free - 100e6);

        uint256 before = usdg.balanceOf(alice);
        uint256 owed = _sell(alice, curve, a);
        assertGt(owed, 100e6);
        // Paid the $100 now, the rest is one claim.
        assertEq(usdg.balanceOf(alice) - before, 100e6);
        assertEq(exchange.queued(), owed - 100e6);
        assertEq(exchange.queueLength(), 1);
        assertEq(exchange.freeFloat(), 0);

        usdg.mint(address(exchange), owed);
        exchange.payQueue(1);
        assertEq(usdg.balanceOf(alice) - before, owed);
        assertEq(exchange.queued(), 0);
    }

    function test_aMintPaysTheQueue() public {
        (uint256 a,) = _buy(alice, curve, 500e6);
        vm.startPrank(keeper);
        exchange.sendToBridge(usdg.balanceOf(address(exchange)));
        vm.stopPrank();
        uint256 before = usdg.balanceOf(alice);
        uint256 owed = _sell(alice, curve, a);
        assertEq(exchange.queueLength(), 1);
        // The next buyer's USDG pays the waiting seller.
        _buy(bob, curve, 1_000e6);
        assertEq(exchange.queueLength(), 0);
        assertEq(usdg.balanceOf(alice), before + owed);
    }

    /* ------------------------------------------------------ after resolution */

    function test_ninetyCentMarketResolvesYesAndTheCoinTradesOn() public {
        // A near-certain market: lift the ceiling for it.
        vm.prank(owner);
        exchange.setMaxPrice(ID, 990_000);
        _post(ID, 970_000);
        (uint256 coins,) = _buy(alice, curve, 1_000e6);

        vm.prank(keeper);
        oracle.settle(ID, 1e6);

        // Buys keep working with no quote at $1 + spread, no backing needed.
        PriceOracle.Quote memory none;
        vm.prank(bob);
        (uint256 bobCoins,) = router.buy(curve, 1_000e6, 0, bob, none, "");
        assertGt(bobCoins, 0);
        // And the posted path works for any terminal, with no posted price and no caps.
        vm.prank(bob);
        router.buyPosted(curve, 10_000e6, 0, bob);
        assertTrue(curve.graduated());

        // Alice bought at 97c and is paid out at $1 less 0.5%.
        uint256 out = _sellPostedAll(alice, coins);
        assertGt(out, 0);
    }

    function _sellPostedAll(address who, uint256 coins) internal returns (uint256 out) {
        vm.startPrank(who);
        coin.approve(address(router), coins);
        out = router.sellPosted(curve, coins, 0, who);
        vm.stopPrank();
    }

    function test_maxPriceOverrideIsPerMarketAndCapped() public {
        _post(ID, 970_000);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PriceOutOfBand.selector, 970_000));
        router.buy(curve, 100e6, 0, alice, q, sig);

        vm.startPrank(owner);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setMaxPrice(ID, 995_000);
        exchange.setMaxPrice(ID, 990_000);
        vm.stopPrank();
        vm.prank(alice);
        router.buy(curve, 100e6, 0, alice, q, sig);
        assertEq(exchange.maxPriceOf(ID_B), exchange.maxPrice());
    }

    /* -------------------------------------------------------------- bursts */

    /// 300 traders pile in and out of one coin within a few blocks, far more than
    /// the float holds. Nothing reverts; every seller is paid or queued exactly;
    /// once the desk's cash comes back, everyone is paid.
    function test_burstOfBuysAndSellsNeverStrandsASeller() public {
        vm.prank(owner);
        exchange.setOutflowCap(1_000_000e6);
        uint256 n = 300;
        address[] memory ts = new address[](n);
        uint256[] memory held = new uint256[](n);
        for (uint256 i; i < n; ++i) {
            ts[i] = address(uint160(0x10000 + i));
            usdg.mint(ts[i], 5_000e6);
            vm.prank(ts[i]);
            usdg.approve(address(router), type(uint256).max);
        }
        // Wave of buys, a few per block, in random sizes: past graduation into the pool.
        for (uint256 i; i < n; ++i) {
            if (i % 5 == 0) vm.roll(block.number + 1);
            (held[i],) = _buy(ts[i], curve, 100e6 + (uint256(keccak256(abi.encode(i))) % 2_000e6));
        }
        assertTrue(curve.graduated());

        // The treasury moved most of the float to the desk to buy shares.
        vm.startPrank(keeper);
        exchange.sendToBridge(exchange.freeFloat() * 9 / 10);
        vm.stopPrank();

        // Everyone dumps at once.
        uint256 owed;
        for (uint256 i; i < n; ++i) {
            if (i % 5 == 0) vm.roll(block.number + 1);
            owed += _sell(ts[i], curve, held[i]);
        }
        assertGt(exchange.queueLength(), 0);

        // The desk sells its shares and the bridge brings the cash back.
        usdg.mint(address(exchange), exchange.queued());
        while (exchange.queueLength() > 0) exchange.payQueue(50);
        assertEq(exchange.queued(), 0);

        uint256 paid;
        for (uint256 i; i < n; ++i) {
            paid += usdg.balanceOf(ts[i]);
        }
        // Everyone's USDG is back: what they did not spend plus exactly what they were owed.
        uint256 spent;
        for (uint256 i; i < n; ++i) {
            spent += 100e6 + (uint256(keccak256(abi.encode(i))) % 2_000e6);
        }
        assertEq(paid, n * 5_000e6 - spent + owed);
    }
}
