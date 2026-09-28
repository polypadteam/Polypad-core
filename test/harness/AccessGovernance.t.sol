// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {Fees} from "../../src/Fees.sol";
import {LaunchFactory} from "../../src/LaunchFactory.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {PolypadBase} from "../Polypad.t.sol";

/**
 * Governance delays at their exact boundaries, every parameter bound at cap and
 * cap + 1, and what a stolen owner or keeper key can do before anyone reacts.
 */
contract AccessGovernanceTest is PolypadBase {
    address internal eve = makeAddr("eve");
    address internal thief = makeAddr("thief");

    /* ------------------------------------------------------- signer rotation */

    function test_signerDelayExactBoundary() public {
        vm.prank(owner);
        oracle.setSigner(eve);
        uint256 at = oracle.pendingSignerAt();
        assertEq(at, block.timestamp + 2 days);
        vm.warp(at - 1);
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.NotYet.selector, at));
        oracle.acceptSigner();
        vm.warp(at);
        vm.prank(owner);
        oracle.acceptSigner();
        assertEq(oracle.signer(), eve);
        assertEq(oracle.pendingSigner(), address(0));
        vm.prank(owner);
        vm.expectRevert(PriceOracle.NothingPending.selector);
        oracle.acceptSigner();
    }

    function test_reproposingASignerRestartsTheClock() public {
        vm.prank(owner);
        oracle.setSigner(eve);
        vm.warp(block.timestamp + 2 days - 1);
        vm.prank(owner);
        oracle.setSigner(bob);
        vm.warp(block.timestamp + 1);
        vm.prank(owner);
        vm.expectRevert();
        oracle.acceptSigner();
        vm.warp(block.timestamp + 2 days);
        vm.prank(owner);
        oracle.acceptSigner();
        assertEq(oracle.signer(), bob);
    }

    function test_revokingTheSignerCancelsAPendingOne() public {
        vm.prank(owner);
        oracle.setSigner(eve);
        vm.prank(owner);
        oracle.setSigner(address(0));
        assertEq(oracle.signer(), address(0));
        vm.warp(block.timestamp + 3 days);
        vm.prank(owner);
        vm.expectRevert(PriceOracle.NothingPending.selector);
        oracle.acceptSigner();
    }

    /* --------------------------------------------------------- bridge deposit */

    function test_bridgeCannotBeRepointedThroughSetRoles() public {
        vm.prank(owner);
        vm.expectRevert(PExchange.BridgeChangeDelayed.selector);
        exchange.setRoles(keeper, address(factory), eve);
        // Same bridge: roles change, bridge stays.
        vm.prank(owner);
        exchange.setRoles(eve, address(factory), bridge);
        assertEq(exchange.keeper(), eve);
        assertEq(exchange.bridgeDeposit(), bridge);
    }

    function test_bridgeDelayExactBoundary() public {
        vm.prank(owner);
        vm.expectRevert(PExchange.NothingPending.selector);
        exchange.acceptBridgeDeposit();
        vm.prank(owner);
        exchange.proposeBridgeDeposit(eve);
        uint256 at = exchange.pendingBridgeDepositAt();
        assertEq(at, block.timestamp + 2 days);
        vm.warp(at - 1);
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotYet.selector, at));
        exchange.acceptBridgeDeposit();
        vm.warp(at);
        vm.prank(owner);
        exchange.acceptBridgeDeposit();
        assertEq(exchange.bridgeDeposit(), eve);
        vm.prank(owner);
        vm.expectRevert(PExchange.NothingPending.selector);
        exchange.acceptBridgeDeposit();
    }

    /// Regression: once a bridge has been set, clearing it (instantly through
    /// setRoles, or through propose/accept) never lets setRoles install a new one
    /// at once: every later bridge waits BRIDGE_DELAY.
    function test_bridgeClearedThenSetStillWaitsTheDelay() public {
        assertTrue(exchange.bridgeEverSet());
        // Clearing through setRoles is instant.
        vm.prank(owner);
        exchange.setRoles(keeper, address(factory), address(0));
        assertEq(exchange.bridgeDeposit(), address(0));
        vm.prank(keeper);
        vm.expectRevert(PExchange.NoBridge.selector);
        exchange.sendToBridge(1);
        vm.prank(owner);
        vm.expectRevert(PExchange.BridgeChangeDelayed.selector);
        exchange.setRoles(keeper, address(factory), eve);

        // Clearing through propose/accept: same.
        vm.prank(owner);
        exchange.proposeBridgeDeposit(eve);
        vm.warp(block.timestamp + 2 days);
        vm.prank(owner);
        exchange.acceptBridgeDeposit();
        assertEq(exchange.bridgeDeposit(), eve);
        vm.prank(owner);
        exchange.proposeBridgeDeposit(address(0));
        vm.warp(block.timestamp + 2 days);
        vm.prank(owner);
        exchange.acceptBridgeDeposit();
        assertEq(exchange.bridgeDeposit(), address(0));
        vm.prank(owner);
        vm.expectRevert(PExchange.BridgeChangeDelayed.selector);
        exchange.setRoles(keeper, address(factory), thief);
    }

    function test_bridgeOnlyEverGetsFreeFloat() public {
        uint256 free = exchange.freeFloat();
        vm.prank(keeper);
        vm.expectRevert(abi.encodeWithSelector(PExchange.InsufficientFloat.selector, free + 1, free));
        exchange.sendToBridge(free + 1);
        vm.prank(keeper);
        exchange.sendToBridge(free);
        assertEq(usdg.balanceOf(bridge), free);
    }

    /* ------------------------------------------------------- parameter bounds */

    function test_setParamsBounds() public {
        vm.startPrank(owner);
        exchange.setParams(500, 500, 1, 999_999, 0);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(501, 25, 50_000, 950_000, 0);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(25, 501, 50_000, 950_000, 0);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(25, 25, 950_000, 950_000, 0);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(25, 25, 50_000, 1_000_000, 0);
        vm.stopPrank();
    }

    function test_setMaxPriceBounds() public {
        vm.startPrank(owner);
        exchange.setMaxPrice(ID, 990_000);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setMaxPrice(ID, 990_001);
        uint64 floor = exchange.minPrice();
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setMaxPrice(ID, floor);
        exchange.setMaxPrice(ID, 0); // reset
        vm.stopPrank();
        assertEq(exchange.maxPriceOf(ID), exchange.maxPrice());
    }

    function test_settleFeeAndOutflowBounds() public {
        vm.startPrank(owner);
        exchange.setSettleFee(200);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setSettleFee(201);
        exchange.setOutflowCap(0, 10_000);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setOutflowCap(0, 10_001);
        vm.stopPrank();
    }

    function test_feeBounds() public {
        vm.startPrank(owner);
        factory.setFees(Fees(200, 3_000, 15_000, 8_000));
        factory.setFees(Fees(1, 8_000, 1, 3_000));
        Fees[8] memory bad = [
            Fees(0, 5_000, 10_000, 5_000),
            Fees(201, 5_000, 10_000, 5_000),
            Fees(140, 2_999, 10_000, 5_000),
            Fees(140, 8_001, 10_000, 5_000),
            Fees(140, 5_000, 0, 5_000),
            Fees(140, 5_000, 15_001, 5_000),
            Fees(140, 5_000, 10_000, 2_999),
            Fees(140, 5_000, 10_000, 8_001)
        ];
        for (uint256 i; i < bad.length; ++i) {
            vm.expectRevert(LaunchFactory.BadFees.selector);
            factory.setFees(bad[i]);
        }
        vm.stopPrank();
    }

    function test_feesChangeOnlyFutureCoins() public {
        (, BondingCurve before) = _launch(ID);
        vm.prank(owner);
        factory.setFees(Fees(200, 3_000, 15_000, 8_000));
        (, BondingCurve later) = _launch(ID);
        assertEq(before.feeBps(), 140);
        assertEq(before.poolFee(), 10_000);
        assertEq(later.feeBps(), 200);
        assertEq(later.creatorShareBps(), 3_000);
    }

    function test_holdersShareBound() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(LaunchFactory.BadHoldersShare.selector, uint16(10_001)));
        factory.launch(ID, "a", "b", "c", 10_001, q, sig);
        vm.prank(creator);
        factory.launch(ID, "a", "b", "c", 10_000, q, sig);
    }

    /* ------------------------------------------------ stolen keys: what's instant */

    /// Regression. The keeper is a hot key, settlement is not checked against
    /// Polymarket on chain, and the keeper also sets the backing that caps mints.
    /// A settlement therefore takes effect only after SETTLE_DELAY, during which
    /// the owner can cancel it: a stolen keeper key can record a $1 payout on a
    /// cheap market, but cannot redeem at it before the owner reacts.
    function test_keeperKeyCannotDrainFloatBySettlingACheapMarket() public {
        (, BondingCurve c) = _launch(ID_B); // a 30c market
        PToken pb = exchange.pTokenOf(ID_B);
        c; // the coin is irrelevant: the thief trades shares directly

        // Keeper (stolen) reports enough backing for a big mint.
        uint256[] memory ids = new uint256[](1);
        uint256[] memory amts = new uint256[](1);
        ids[0] = ID_B;
        amts[0] = 1_000_000e6;
        vm.prank(keeper);
        exchange.reportBacked(ids, amts);

        // The thief buys 10k USDG of 30c shares at a legit quote.
        usdg.mint(thief, 10_000e6);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID_B, BUY);
        vm.startPrank(thief);
        usdg.approve(address(exchange), 10_000e6);
        uint256 shares = exchange.mint(address(pb), 10_000e6, 0, thief, q, sig);
        vm.stopPrank();

        // Keeper settles the 30c market as a win, the same block: only recorded.
        vm.prank(keeper);
        oracle.settle(ID_B, 1e6);
        (, bool settled, uint64 payout) = oracle.status(ID_B);
        assertFalse(settled);
        assertEq(payout, 0);
        (uint64 recorded, uint64 settleAt) = oracle.settlement(ID_B);
        assertEq(recorded, 1e6);
        assertEq(settleAt, block.timestamp + oracle.SETTLE_DELAY());

        // Until it takes effect, a redemption is priced by a live SELL quote (30c),
        // so the round trip only loses the spreads.
        vm.warp(settleAt - 1);
        (q, sig) = signedQuote(ID_B, SELL);
        vm.prank(thief);
        exchange.redeem(address(pb), shares / 3, 0, thief, q, sig);
        assertLt(usdg.balanceOf(thief), uint256(10_000e6) / 3 + 1);

        // The owner vetoes it before it takes effect.
        vm.prank(owner);
        oracle.cancelSettle(ID_B);
        (recorded, settleAt) = oracle.settlement(ID_B);
        assertEq(recorded, 0);
        assertEq(settleAt, 0);
        vm.warp(block.timestamp + 2 * oracle.SETTLE_DELAY());
        (, settled,) = oracle.status(ID_B);
        assertFalse(settled);
        // Still no $1 redemption: the SELL quote prices it at 30c.
        uint256 before = usdg.balanceOf(thief);
        (q, sig) = signedQuote(ID_B, SELL);
        uint256 rest = pb.balanceOf(thief);
        vm.prank(thief);
        uint256 out = exchange.redeem(address(pb), rest, 0, thief, q, sig);
        assertEq(usdg.balanceOf(thief) - before, out);
        assertLt(usdg.balanceOf(thief), 10_000e6, "no hot key pulled float out");

        // A cancel after it has taken effect is refused: settlement is one-way then.
        vm.prank(keeper);
        oracle.settle(ID_B, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.prank(owner);
        vm.expectRevert(PriceOracle.NothingPending.selector);
        oracle.cancelSettle(ID_B);
    }

    function test_cancelSettleIsOwnerOnlyAndNeedsAPendingSettlement() public {
        vm.prank(owner);
        vm.expectRevert(PriceOracle.NothingPending.selector);
        oracle.cancelSettle(ID);
        vm.prank(keeper);
        oracle.settle(ID, 1e6);
        vm.prank(keeper);
        vm.expectRevert();
        oracle.cancelSettle(ID);
        vm.prank(owner);
        oracle.cancelSettle(ID);
        // Cancelled: the keeper may record the right payout.
        vm.prank(keeper);
        oracle.settle(ID, 0);
        (uint64 recorded,) = oracle.settlement(ID);
        assertEq(recorded, 0);
    }

    /// Owner key: becomes oracle keeper in one call, no delay.
    function test_ownerCanBecomeOracleKeeperInstantly() public {
        vm.prank(owner);
        oracle.setKeeper(thief);
        vm.prank(thief);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        (, bool settled, uint64 payout) = oracle.status(ID);
        assertTrue(settled);
        assertEq(payout, 1e6);
    }

    function test_settlementIsOneWayAndBounded() public {
        vm.prank(keeper);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID, uint64(1e6 + 1)));
        oracle.settle(ID, 1e6 + 1);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.prank(keeper);
        oracle.settle(ID, 0);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.prank(keeper);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.AlreadySettled.selector, ID));
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
    }

    function test_outflowCapIsTheOnlyBrakeAndOwnerCanLiftItInstantly() public {
        vm.prank(owner);
        exchange.setOutflowCap(type(uint256).max, 0);
        assertEq(exchange.outflowRemaining(), type(uint256).max);
        vm.prank(owner);
        exchange.setOutflowCap(0, 0);
        assertEq(exchange.outflowRemaining(), 0);
    }

    /// The cap is the larger of the floor and a share of the float.
    function test_outflowCapScalesWithTheFloatAboveTheFloor() public {
        uint256 bal = usdg.balanceOf(address(exchange));
        vm.prank(owner);
        exchange.setOutflowCap(1_000e6, 5_000);
        assertEq(exchange.outflowRemaining(), (bal * 5_000) / 10_000);
        vm.prank(owner);
        exchange.setOutflowCap(bal * 2, 5_000);
        assertEq(exchange.outflowRemaining(), bal * 2, "the floor wins over a small float");
    }

    /// Regression: renouncing would leave a halted exchange unresumable forever,
    /// so it is disabled on every owned contract.
    function test_renounceOwnershipIsDisabled() public {
        vm.startPrank(owner);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.renounceOwnership();
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.renounceOwnership();
        vm.expectRevert(LaunchFactory.BadParams.selector);
        factory.renounceOwnership();
        vm.stopPrank();
        assertEq(exchange.owner(), owner);
        assertEq(oracle.owner(), owner);
        assertEq(factory.owner(), owner);
        vm.prank(keeper);
        exchange.halt();
        vm.prank(owner);
        exchange.resume();
        assertFalse(exchange.halted());
    }

    function test_rescueNeverTouchesFloatOrShares() public {
        (Coin coin,) = _launch(ID);
        PToken p = exchange.pTokenOf(ID);
        vm.startPrank(owner);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotRescuable.selector, address(usdg)));
        exchange.rescue(IERC20(address(usdg)), owner, 1);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotRescuable.selector, address(p)));
        exchange.rescue(IERC20(address(p)), owner, 1);
        vm.stopPrank();
        coin; // coins sent here by mistake are rescuable, which is intended
    }

    function test_keeperCanBeChangedInstantlyOnBoth() public {
        vm.startPrank(owner);
        exchange.setRoles(thief, address(factory), bridge);
        oracle.setKeeper(thief);
        vm.stopPrank();
        assertEq(exchange.keeper(), thief);
        assertEq(oracle.keeper(), thief);
    }
}
