// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Vm} from "forge-std/Vm.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {MockUSDG, PolypadBase} from "../Polypad.t.sol";

/// @dev Recipient that can never receive USDG and cannot call back (e.g. a frozen contract).
contract StrayToken is ERC20 {
    constructor() ERC20("Stray", "STRAY") {
        _mint(msg.sender, 1e24);
    }
}

/**
 * Pre-audit harness: PExchange + PriceOracle timing, races, caps and the queue.
 * Direct calls to the exchange (no curve), so every number is the exchange's own.
 */
contract ExchangeTimingTest is PolypadBase {
    address internal carol = makeAddr("carol");
    PToken internal pA;
    PToken internal pB;

    function setUp() public override {
        super.setUp();
        vm.startPrank(owner);
        pA = exchange.ensurePToken(ID);
        pB = exchange.ensurePToken(ID_B);
        vm.stopPrank();
        usdg.mint(carol, 100_000e6);
        address[3] memory who = [alice, bob, carol];
        for (uint256 i; i < 3; ++i) {
            vm.prank(who[i]);
            usdg.approve(address(exchange), type(uint256).max);
        }
        _postOnChain(ID, 600_000);
        _postOnChain(ID_B, 300_000);
        // The risk cap is out of the way unless a test turns it on (`_riskOn`).
        // (Backing cannot be reported above supply, so it cannot be used for this.)
        vm.startPrank(owner);
        exchange.setMaxRisk(ID, type(uint256).max);
        exchange.setMaxRisk(ID_B, type(uint256).max);
        vm.stopPrank();
    }

    /// @dev The default $1,000 risk cap, nothing backed.
    function _riskOn(uint256 id) internal {
        vm.prank(owner);
        exchange.setMaxRisk(id, 0);
        _back(id, 0);
    }

    /* ------------------------------------------------------------ helpers */

    function _q(uint256 id, uint8 side, uint64 price, uint256 maxAmount, uint64 validUntil)
        internal
        view
        returns (PriceOracle.Quote memory q, bytes memory sig)
    {
        q = PriceOracle.Quote(id, side, price, maxAmount, validUntil);
        sig = _sign(q, signerPk);
    }

    function _qNow(uint256 id, uint8 side) internal view returns (PriceOracle.Quote memory, bytes memory) {
        return _q(id, side, px[id], type(uint256).max, uint64(block.timestamp + 15));
    }

    function _mint(address who, uint256 id, uint256 usdgIn) internal returns (uint256) {
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(id, BUY);
        address p = address(exchange.pTokenOf(id));
        vm.prank(who);
        return exchange.mint(p, usdgIn, 0, who, q, sig);
    }

    function _redeem(address who, uint256 id, uint256 amount) internal returns (uint256) {
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(id, SELL);
        address p = address(exchange.pTokenOf(id));
        vm.prank(who);
        return exchange.redeem(p, amount, 0, who, q, sig);
    }

    /// @dev Move the price the pricer quotes.
    function _postOnChain(uint256 id, uint64 price) internal {
        _post(id, price);
    }

    function _settle(uint256 id, uint64 payout) internal {
        vm.prank(keeper);
        oracle.settle(id, payout);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
    }

    function _drainFloat() internal {
        uint256 free = exchange.freeFloat();
        vm.prank(keeper);
        exchange.sendToBridge(free);
    }

    /* =============================================== 1. signed quote timing */

    function test_quoteUsableThroughItsLastSecondNotAfter() public {
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, 600_000, 1e12, uint64(block.timestamp + 10));
        vm.warp(block.timestamp + 10); // == validUntil: still good
        vm.prank(alice);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        vm.warp(block.timestamp + 1);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteExpired.selector, q.validUntil));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
    }

    function test_quoteValidityCappedAtThirtySeconds() public {
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, 600_000, 1e12, uint64(block.timestamp + 30));
        vm.prank(alice);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        (q, sig) = _q(ID, BUY, 600_000, 1e12, uint64(block.timestamp + 31));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteTooLong.selector, q.validUntil));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        // A long quote becomes usable once it is within 30s of expiry: the cap is on
        // remaining life, not on how long ago it was signed.
        vm.warp(block.timestamp + 1);
        vm.prank(alice);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
    }

    function test_quoteForTheOtherSideIsRefused() public {
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        uint256 shares = _mint(alice, ID, 10e6);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.WrongSide.selector, BUY));
        exchange.redeem(address(pA), shares, 0, alice, q, sig);
        (q, sig) = _qNow(ID, SELL);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.WrongSide.selector, SELL));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
    }

    function test_quoteForAnotherMarketIsRefused() public {
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID_B, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteForOtherMarket.selector, ID_B, ID));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
    }

    function test_quoteMaxAmountIsInclusive() public {
        // One unit over, on a fresh quote: refused.
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, 600_000, 50e6, uint64(block.timestamp + 15));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteTooSmall.selector, 50e6 + 1, 50e6));
        exchange.mint(address(pA), 50e6 + 1, 0, alice, q, sig);
        // Exactly maxAmount: fine, and it uses the quote up.
        vm.prank(alice);
        exchange.mint(address(pA), 50e6, 0, alice, q, sig);
        assertEq(exchange.quoteFilled(oracle.quoteDigest(q)), 50e6);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteTooSmall.selector, 50e6 + 1, 50e6));
        exchange.mint(address(pA), 1, 0, alice, q, sig);
    }

    function test_quoteFromAnyoneButTheSignerIsRefused() public {
        PriceOracle.Quote memory q = PriceOracle.Quote(ID, BUY, 600_000, 1e12, uint64(block.timestamp + 15));
        bytes memory sig = _sign(q, 0xBAD);
        vm.prank(alice);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
    }

    function test_editingAnySignedFieldBreaksTheSignature() public {
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, 600_000, 100e6, uint64(block.timestamp + 15));
        PriceOracle.Quote memory t = q;
        t.price = 500_000;
        vm.prank(alice);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        exchange.mint(address(pA), 10e6, 0, alice, t, sig);
        t = PriceOracle.Quote(q.positionId, q.side, q.price, q.maxAmount * 10, q.validUntil);
        vm.prank(alice);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        exchange.mint(address(pA), 10e6, 0, alice, t, sig);
        t = PriceOracle.Quote(q.positionId, q.side, q.price, q.maxAmount, q.validUntil + 5);
        vm.prank(alice);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        exchange.mint(address(pA), 10e6, 0, alice, t, sig);
    }

    function test_quotePriceMustBeStrictlyInsideZeroAndOne() public {
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, SELL, 0, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID, 0));
        exchange.redeem(address(pA), 1, 0, alice, q, sig);
        (q, sig) = _q(ID, SELL, 1e6, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID, 1e6));
        exchange.redeem(address(pA), 1, 0, alice, q, sig);
    }

    function test_aQuoteForOneOracleIsWorthlessAtAnother() public {
        // Same signer, a second deployment: EIP-712 binds the quote to its verifying contract.
        PriceOracle other = new PriceOracle(owner, signer, keeper);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        other.verify(q, sig, BUY);
        assertEq(oracle.verify(q, sig, BUY), 600_000);
    }

    function test_signerRotationWaitsTheDelayAndCutsOverCleanly() public {
        uint256 newPk = 0xB0B0;
        address newSigner = vm.addr(newPk);
        vm.prank(owner);
        oracle.setSigner(newSigner);
        uint256 at = oracle.pendingSignerAt();
        assertEq(at, block.timestamp + oracle.ROLE_DELAY());

        PriceOracle.Quote memory q = PriceOracle.Quote(ID, BUY, 600_000, 1e12, uint64(block.timestamp + 15));
        bytes memory oldSig = _sign(q, signerPk);
        bytes memory newSig = _sign(q, newPk);
        // Pending signer is not yet trusted; the old one still is.
        vm.expectRevert(PriceOracle.BadSignature.selector);
        oracle.verify(q, newSig, BUY);
        oracle.verify(q, oldSig, BUY);

        vm.warp(at - 1);
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.NotYet.selector, at));
        oracle.acceptSigner();
        vm.warp(at);
        vm.prank(owner);
        oracle.acceptSigner();

        q.validUntil = uint64(block.timestamp + 15);
        oldSig = _sign(q, signerPk);
        newSig = _sign(q, newPk);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        oracle.verify(q, oldSig, BUY);
        oracle.verify(q, newSig, BUY);

        vm.prank(owner);
        vm.expectRevert(PriceOracle.NothingPending.selector);
        oracle.acceptSigner();
    }

    function test_revokingTheSignerIsImmediateAndKillsEveryQuote() public {
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(owner);
        oracle.setSigner(address(0));
        vm.prank(alice);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        // And a pending rotation is cancelled by the revoke.
        assertEq(oracle.pendingSigner(), address(0));
    }

    function test_onlyOwnerManagesRoles() public {
        vm.startPrank(alice);
        vm.expectRevert();
        oracle.setSigner(alice);
        vm.expectRevert();
        oracle.setKeeper(alice);
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.setPaused(new uint256[](0), true);
        vm.stopPrank();
    }

    /// Documented behaviour: a quote is a price, not a ticket. It may be used any
    /// number of times until it expires, each use bounded by `maxAmount`; the
    /// unbacked cap and the outflow cap bound the total.
    /// v8: a quote covers `maxAmount` in total across every use, by anyone.
    function test_aQuoteCoversMaxAmountInTotalAcrossReplays() public {
        _riskOn(ID);
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, 600_000, 300e6, uint64(block.timestamp + 15));
        vm.startPrank(alice);
        exchange.mint(address(pA), 100e6, 0, alice, q, sig);
        exchange.mint(address(pA), 200e6, 0, alice, q, sig);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteTooSmall.selector, 300e6 + 1, 300e6));
        exchange.mint(address(pA), 1, 0, alice, q, sig);
        vm.stopPrank();
        // It binds no taker, but a replay by someone else draws on the same total.
        vm.prank(bob);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteTooSmall.selector, 300e6 + 300e6, 300e6));
        exchange.mint(address(pA), 300e6, 0, bob, q, sig);
        assertEq(exchange.quoteFilled(oracle.quoteDigest(q)), 300e6);
    }

    function test_quoteFillsAreTrackedPerQuote() public {
        (PriceOracle.Quote memory q1, bytes memory s1) = _q(ID, BUY, 600_000, 100e6, uint64(block.timestamp + 15));
        (PriceOracle.Quote memory q2, bytes memory s2) = _q(ID, BUY, 600_000, 100e6 + 1, uint64(block.timestamp + 15));
        (PriceOracle.Quote memory q3, bytes memory s3) = _q(ID_B, BUY, 600_000, 100e6, uint64(block.timestamp + 15));
        vm.startPrank(alice);
        exchange.mint(address(pA), 100e6, 0, alice, q1, s1);
        // A different maxAmount (the pricer adds a few wei) is a different quote.
        exchange.mint(address(pA), 100e6, 0, alice, q2, s2);
        exchange.mint(address(pB), 100e6, 0, alice, q3, s3);
        vm.stopPrank();
        assertEq(exchange.quoteFilled(oracle.quoteDigest(q1)), 100e6);
        assertEq(exchange.quoteFilled(oracle.quoteDigest(q2)), 100e6);
        assertEq(exchange.quoteFilled(oracle.quoteDigest(q3)), 100e6);
        assertTrue(oracle.quoteDigest(q1) != oracle.quoteDigest(q2));
    }

    /* =============================================== 2. pause and settlement */

    function test_pausedMarketStopsBuysNotSells() public {
        uint256 shares = _mint(alice, ID, 100e6);
        uint256[] memory ids = new uint256[](1);
        ids[0] = ID;
        vm.prank(keeper);
        oracle.setPaused(ids, true);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        assertGt(_redeem(alice, ID, shares / 2), 0);
        // Unpause reopens everything.
        vm.prank(keeper);
        oracle.setPaused(ids, false);
        assertGt(_mint(alice, ID, 10e6), 0);
    }

    function test_settledMarketTradesAtPayoutQuotesIgnored() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _settle(ID, 1e6);
        // A live quote is refused on a settled market...
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, SELL);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketSettled.selector, ID));
        oracle.verify(q, sig, SELL);
        // ...but the exchange routes settled redemptions to the payout, quote ignored.
        PriceOracle.Quote memory junk;
        uint256 before = usdg.balanceOf(alice);
        vm.prank(alice);
        uint256 out = exchange.redeem(address(pA), shares / 2, 0, alice, junk, "");
        assertEq(out, ((shares / 2) * ((1e6 * uint256(10_000 - exchange.settleFeeBps())) / 10_000)) / 1e6);
        assertEq(usdg.balanceOf(alice) - before, out);
        vm.prank(alice);
        uint256 out2 = exchange.redeem(address(pA), shares / 2, 0, alice, q, sig);
        assertEq(out2, out);
        // Settled mints need no quote either.
        vm.prank(alice);
        exchange.mint(address(pA), 5_000e6, 0, alice, junk, "");
    }

    function test_settleIsOneWayAndBounded() public {
        vm.startPrank(keeper);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID, 1e6 + 1));
        oracle.settle(ID, 1e6 + 1);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        oracle.settle(ID, 0);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.AlreadySettled.selector, ID));
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.stopPrank();
    }

    /* =============================================== 3. outflow cap */

    function _toLastSecondOfHour() internal {
        uint256 t = (block.timestamp / 3_600 + 1) * 3_600 - 1;
        vm.warp(t);
    }

    /// A fixed hourly cap: `floor` with no share of the float.
    function _fixedCap(uint256 cap) internal {
        vm.prank(owner);
        exchange.setOutflowCap(cap, 0);
    }

    function _delayed(uint256 ticket)
        internal
        view
        returns (address to, uint64 readyAt, uint256 id, uint256 pAmount, uint256 amount)
    {
        (to, readyAt, id, pAmount, amount) = exchange.delayed(ticket);
    }

    function test_outflowCapExactFillThenHourRollover() public {
        uint256 shares = _mint(alice, ID, 10_000e6);
        uint256 amount = shares / 4;
        uint256 out = exchange.redeemOut(600_000, amount);
        _fixedCap(out * 2);
        _toLastSecondOfHour();
        _redeem(alice, ID, amount);
        _redeem(alice, ID, amount); // exactly fills the hour
        assertEq(exchange.outflowRemaining(), 0);
        // v9: past the cap a sale is not refused. All of it waits.
        uint256 before = usdg.balanceOf(alice);
        // (At least MIN_DELAYED: a smaller over-cap part reverts DustOverCap.)
        uint256 over = _redeem(alice, ID, 2e6);
        assertEq(over, exchange.redeemOut(600_000, 2e6));
        assertEq(usdg.balanceOf(alice), before, "an over-cap sale was paid at once");
        (address to, uint64 readyAt, uint256 id, uint256 pAmount, uint256 owed) = _delayed(0);
        assertEq(to, alice);
        assertEq(readyAt, block.timestamp + exchange.DELAY());
        assertEq(id, ID);
        assertEq(pAmount, 2e6);
        assertEq(owed, over);
        assertEq(exchange.delayedShares(ID), 2e6);
        assertEq(pA.balanceOf(address(exchange)), 2e6);
        // A sliding hour. One second later is a new clock hour, but the last one
        // still counts in full: no fresh cap at the boundary.
        vm.warp(block.timestamp + 1);
        assertEq(exchange.outflowRemaining(), 0);
        // It frees linearly: half the cap half an hour on, all of it an hour on.
        vm.warp(block.timestamp + 1_800);
        assertApproxEqAbs(exchange.outflowRemaining(), out, 1);
        vm.warp(block.timestamp + 1_800);
        assertEq(exchange.outflowRemaining(), out * 2);
        before = usdg.balanceOf(alice);
        _redeem(alice, ID, amount);
        assertEq(usdg.balanceOf(alice) - before, out, "a sale under the cap was not paid in full");
    }

    function test_saleOverTheCapIsSplitAtItsPrice() public {
        uint256 shares = _mint(alice, ID, 10_000e6);
        _fixedCap(1_000e6);
        uint256 before = usdg.balanceOf(alice);
        uint256 out = _redeem(alice, ID, shares);
        assertEq(out, exchange.redeemOut(600_000, shares));
        // The cap's worth is paid now; the rest, and the pToken for it, wait.
        assertEq(usdg.balanceOf(alice) - before, 1_000e6);
        (,,, uint256 pAmount, uint256 owed) = _delayed(0);
        assertEq(owed, out - 1_000e6);
        assertEq(pAmount, (shares * (out - 1_000e6)) / out);
        assertEq(pA.totalSupply(), pAmount, "the paid part's pToken was not burned");
        assertEq(pA.balanceOf(alice), 0);
        // The price is fixed: a move before release changes nothing.
        _postOnChain(ID, 100_000);
        vm.warp(block.timestamp + exchange.DELAY());
        before = usdg.balanceOf(alice);
        vm.prank(carol); // anyone may release
        exchange.release(0);
        assertEq(usdg.balanceOf(alice) - before, owed);
        assertEq(pA.totalSupply(), 0);
        assertEq(exchange.delayedShares(ID), 0);
    }

    function test_releaseWaitsTheDelayOnceOnlyAndNotWhileHalted() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _fixedCap(1);
        _redeem(alice, ID, shares);
        (, uint64 readyAt,,,) = _delayed(0);
        vm.warp(readyAt - 1);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotYet.selector, readyAt));
        exchange.release(0);
        vm.warp(readyAt);
        vm.prank(keeper);
        exchange.halt();
        vm.expectRevert(PExchange.Halted.selector);
        exchange.release(0);
        vm.prank(owner);
        exchange.resume();
        exchange.release(0);
        vm.expectRevert(PExchange.NothingPending.selector);
        exchange.release(0);
        vm.prank(owner);
        vm.expectRevert(PExchange.NothingPending.selector);
        exchange.cancel(0);
        vm.expectRevert();
        exchange.release(1); // never existed
    }

    function test_releaseQueuesWhatTheFloatCannotPay() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _fixedCap(1);
        uint256 out = _redeem(alice, ID, shares);
        _drainFloat();
        vm.warp(block.timestamp + exchange.DELAY());
        exchange.release(0);
        (, uint64 readyAt,,,) = _delayed(0);
        assertEq(readyAt, 0, "a released sale is still on the books");
        // Paid in turn like any sale the float could not cover.
        assertEq(exchange.queued(), out - 1);
    }

    function test_ownerCancelGivesThePTokenBack() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _fixedCap(1);
        uint256 before = usdg.balanceOf(alice);
        _redeem(alice, ID, shares);
        (,,, uint256 pAmount,) = _delayed(0);
        vm.prank(alice);
        vm.expectRevert();
        exchange.cancel(0);
        vm.prank(keeper);
        vm.expectRevert();
        exchange.cancel(0);
        // Cancelling works while halted: that is when it is needed.
        vm.prank(keeper);
        exchange.halt();
        vm.prank(owner);
        exchange.cancel(0);
        assertEq(pA.balanceOf(alice), pAmount);
        assertEq(usdg.balanceOf(alice) - before, 1, "only the 1-wei cap was paid");
        assertEq(exchange.delayedShares(ID), 0);
        vm.warp(block.timestamp + exchange.DELAY());
        vm.prank(owner);
        exchange.resume();
        vm.expectRevert(PExchange.NothingPending.selector);
        exchange.release(0);
    }

    function test_absorbLeavesDelayedSharesAlone() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _fixedCap(1);
        _redeem(alice, ID, shares);
        (,,, uint256 pAmount,) = _delayed(0);
        assertEq(exchange.absorb(address(pA)), 0);
        // A fee sent here is absorbed; the delayed sale's pToken stays.
        uint256 fee = _mint(bob, ID, 10e6);
        vm.prank(bob);
        pA.transfer(address(exchange), fee);
        assertEq(exchange.absorb(address(pA)), fee);
        assertEq(pA.balanceOf(address(exchange)), pAmount);
        vm.warp(block.timestamp + exchange.DELAY());
        exchange.release(0);
    }

    function test_capScalesWithTheFloatAndNeverBelowTheFloor() public {
        vm.prank(owner);
        exchange.setOutflowCap(1_000e6, 5_000);
        // Before the hour's first trade the cap follows the free float.
        uint256 free = exchange.freeFloat();
        assertEq(exchange.outflowRemaining(), (free * 5_000) / 10_000);
        // The hour's first trade fixes it at the float before that trade: a mint
        // does not raise it, and paying out does not shrink it.
        uint256 shares = _mint(alice, ID, 10_000e6);
        assertEq(exchange.outflowRemaining(), (free * 5_000) / 10_000);
        uint256 out = _redeem(alice, ID, shares / 2);
        assertEq(exchange.outflowRemaining(), (free * 5_000) / 10_000 - out);
        // The desk taking the float shrinks the next hours' cap, down to the floor.
        _drainFloat();
        vm.warp(block.timestamp + 2 hours);
        assertEq(exchange.freeFloat(), 0);
        assertEq(exchange.outflowRemaining(), 1_000e6);
        vm.prank(owner);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setOutflowCap(0, 100_001);
        vm.prank(alice);
        vm.expectRevert();
        exchange.setOutflowCap(0, 0);
    }

    function test_slidingOutflowWindowMath() public {
        _mint(alice, ID, 10_000e6);
        _fixedCap(1_000e6);
        // Start of an hour, then use 600 of the cap at minute 10.
        vm.warp((block.timestamp / 3_600 + 1) * 3_600 + 600);
        uint256 used;
        while (used < 600e6) used += _redeem(alice, ID, 10e6);
        uint256 cap = 1_000e6;
        assertEq(exchange.outflowRemaining(), cap - used);
        // Same hour: nothing frees.
        vm.warp(block.timestamp + 2_000);
        assertEq(exchange.outflowRemaining(), cap - used);
        // Into the next hour by t seconds: the old hour counts (3600 - t) / 3600.
        uint256 hourStart = (block.timestamp / 3_600 + 1) * 3_600;
        uint256[4] memory ts = [uint256(0), 900, 2_700, 3_599];
        for (uint256 i; i < ts.length; ++i) {
            vm.warp(hourStart + ts[i]);
            uint256 expectUsed = (used * (3_600 - ts[i])) / 3_600;
            assertEq(exchange.outflowRemaining(), cap - expectUsed);
        }
        // Two hours on, the old hour is gone.
        vm.warp(hourStart + 3_600);
        assertEq(exchange.outflowRemaining(), cap);
    }

    function test_outflowCapCountsSignedSettledAndQueuedAlike() public {
        uint256 shares = _mint(alice, ID, 4_000e6);
        uint256 sharesB = _mint(alice, ID_B, 1_000e6);
        _fixedCap(1_000_000e6);
        uint256 used0 = 1_000_000e6 - exchange.outflowRemaining();
        uint256 a = _redeem(alice, ID, shares / 4);
        _drainFloat();
        uint256 c = _redeem(alice, ID, shares / 4); // queued entirely
        assertEq(exchange.queued(), c);
        assertEq(1_000_000e6 - exchange.outflowRemaining() - used0, a + c);
        // A settlement takes SETTLE_DELAY, which rolls into a later clock hour:
        // the settled redemption is the only outflow of that hour.
        _settle(ID_B, 1e6);
        // The sliding hour still sees part of the earlier one; an hour more clears it.
        vm.warp(block.timestamp + 3_600);
        assertEq(exchange.outflowRemaining(), 1_000_000e6);
        PriceOracle.Quote memory junk;
        vm.prank(alice);
        uint256 d = exchange.redeem(address(pB), sharesB, 0, alice, junk, "");
        assertEq(1_000_000e6 - exchange.outflowRemaining(), d);
    }

    function test_loweringTheCapBelowUsedClosesTheHourWithoutUnderflow() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _redeem(alice, ID, shares / 2);
        _fixedCap(1);
        assertEq(exchange.outflowRemaining(), 0);
        uint256 before = usdg.balanceOf(alice);
        uint256 out = _redeem(alice, ID, 2e6);
        assertEq(usdg.balanceOf(alice), before, "a sale with the hour closed was paid at once");
        (,,,, uint256 owed) = _delayed(0);
        assertEq(owed, out);
    }

    /* =============================================== 4. unbacked risk cap */

    function test_riskCapExactBoundary() public {
        _riskOn(ID);
        // At 60c a share can rise 40c: $1,000 of risk is 2,500 unbacked shares.
        // At 60c + 0.25% the share costs 0.6015: 1,503.75 USDG buys exactly 2,500.
        assertEq(exchange.mintOut(600_000, 1_503_750_000), 2_500e6);
        _mint(alice, ID, 1_503_750_000);
        assertEq((pA.totalSupply() * 400_000) / 1e6, exchange.defaultMaxRisk());
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        uint256 more = exchange.mintOut(600_000, 1e6);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.RiskCap.selector, ((2_500e6 + more) * 400_000) / 1e6, 1_000e6));
        exchange.mint(address(pA), 1e6, 0, alice, q, sig);
    }

    function test_riskCapLetsNearCertainMarketsGoFurtherUnbacked() public {
        _riskOn(ID);
        // At 98c a share can rise 2c: $1,000 of risk is 50,000 unbacked shares.
        _postOnChain(ID, 980_000);
        uint256 got = _mint(alice, ID, 40_000e6);
        assertGt(got, 40_000e6);
        // The same dollars at 20c would be 25x the risk: refused.
        _postOnChain(ID_B, 200_000);
        _riskOn(ID_B);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID_B, BUY);
        vm.prank(alice);
        vm.expectRevert();
        exchange.mint(address(pB), 1_000e6, 0, alice, q, sig);
        // It is judged at the quoted price: a quote signed low buys less room.
        assertGt(_mint(alice, ID_B, 200e6), 0);
    }

    function test_riskStillCountsSharesHeldForDelayedSales() public {
        _riskOn(ID);
        uint256 shares = _mint(alice, ID, 1_503_750_000); // at the cap
        _fixedCap(1);
        _redeem(alice, ID, shares); // all but 1 wei's worth delayed, held by the exchange
        assertGt(exchange.delayedShares(ID), 0);
        // A cancel would hand them back, so they still take up the room.
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(bob);
        vm.expectPartialRevert(PExchange.RiskCap.selector);
        exchange.mint(address(pA), 1_000e6, 0, bob, q, sig);
    }

    function test_backingMovesTheCapAndSellsStillWorkWhenOverIt() public {
        _riskOn(ID);
        uint256 shares = _mint(alice, ID, 1_503_750_000);
        _back(ID, 1_000e6);
        uint256 more = _mint(alice, ID, 601_500_000); // exactly 1,000 more shares
        assertEq(more, 1_000e6);
        // Desk reports less than it had: supply is now well over backing + cap.
        _riskOn(ID);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(alice);
        vm.expectRevert();
        exchange.mint(address(pA), 1e6, 0, alice, q, sig);
        // Selling never depends on backing.
        assertGt(_redeem(alice, ID, shares), 0);
    }

    function test_perMarketRiskOverrideAndItsZeroMeansDefault() public {
        _riskOn(ID);
        vm.prank(owner);
        exchange.setMaxRisk(ID, 40e6); // 100 shares at 60c
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(alice);
        vm.expectRevert();
        exchange.mint(address(pA), 100e6, 0, alice, q, sig);
        _mint(alice, ID, 60e6);
        // Setting 0 does NOT close the market: it restores the $1,000 default.
        // (To stop mints on one market use pause, or a 1-wei cap.)
        vm.prank(owner);
        exchange.setMaxRisk(ID, 0);
        assertEq(exchange.maxRisk(ID), exchange.defaultMaxRisk());
        _mint(alice, ID, 600e6);
    }

    function test_settledMintsBypassTheRiskCap() public {
        _fixedCap(1_000_000e6);
        _riskOn(ID);
        _settle(ID, 1e6);
        PriceOracle.Quote memory junk;
        vm.prank(alice);
        uint256 out = exchange.mint(address(pA), 50_000e6, 0, alice, junk, "");
        assertGt(out, 2_500e6);
        // Minted at payout + buy spread; redeemed at payout - settle fee: no round-trip profit.
        uint256 before = usdg.balanceOf(alice);
        vm.prank(alice);
        uint256 back = exchange.redeem(address(pA), out, 0, alice, junk, "");
        assertLt(back, 50_000e6);
        assertEq(usdg.balanceOf(alice) - before, back);
    }

    function test_onlyKeeperReportsBacking() public {
        vm.prank(alice);
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.reportBacked(new uint256[](1), new uint256[](1));
        vm.prank(keeper);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.reportBacked(new uint256[](1), new uint256[](2));
    }

    /* =============================================== 5. queue */

    function test_queueFifoAcrossManySellersPartialFloatFuzz(uint256 seed) public {
        uint256 n = 3 + (seed % 6);
        address[] memory who = new address[](n);
        uint256[] memory shares = new uint256[](n);
        for (uint256 i; i < n; ++i) {
            who[i] = makeAddr(string(abi.encodePacked("seller", vm.toString(i))));
            usdg.mint(who[i], 10_000e6);
            vm.prank(who[i]);
            usdg.approve(address(exchange), type(uint256).max);
            shares[i] = _mint(who[i], ID, 100e6 + ((seed >> (i * 8)) % 900) * 1e6);
        }
        _drainFloat();
        // Leave a little float so the first seller is paid in part.
        usdg.mint(address(exchange), 37e6);
        uint256[] memory owed = new uint256[](n);
        uint256[] memory start = new uint256[](n);
        for (uint256 i; i < n; ++i) {
            start[i] = usdg.balanceOf(who[i]);
            owed[i] = _redeem(who[i], ID, shares[i]);
        }
        assertEq(exchange.freeFloat(), 0);
        // Refill in random slices; claims must be paid strictly in order.
        uint256 total;
        for (uint256 i; i < n; ++i) {
            total += owed[i];
        }
        uint256 paidSoFar = 37e6;
        while (exchange.queueLength() > 0) {
            uint256 slice = 1e6 + (uint256(keccak256(abi.encode(seed, paidSoFar))) % 700e6);
            usdg.mint(address(exchange), slice);
            paidSoFar += slice;
            exchange.payQueue(1 + (seed % 3));
            uint256 head = exchange.claimHead();
            for (uint256 i; i < n; ++i) {
                (address to, uint96 amt) = exchange.claims(i);
                if (i < head) assertEq(amt, 0, "a paid claim is still on the books");
                else assertEq(to, who[i], "queue order changed");
            }
        }
        for (uint256 i; i < n; ++i) {
            assertEq(usdg.balanceOf(who[i]) - start[i], owed[i], "a seller got more or less than owed");
        }
        assertEq(exchange.queued(), 0);
    }

    function test_frozenSellersInTheMiddleAreSetAsideOthersPaidInOrder() public {
        uint256 a = _mint(alice, ID, 500e6);
        uint256 b = _mint(bob, ID, 500e6);
        uint256 c = _mint(carol, ID, 500e6);
        _drainFloat();
        uint256 oa = _redeem(alice, ID, a);
        uint256 ob = _redeem(bob, ID, b);
        uint256 oc = _redeem(carol, ID, c);
        usdg.freeze(bob);
        uint256 a0 = usdg.balanceOf(alice);
        uint256 c0 = usdg.balanceOf(carol);
        usdg.mint(address(exchange), oa + ob + oc);
        assertEq(exchange.payQueue(10), 3, "an undelivered claim counts as processed");
        assertEq(usdg.balanceOf(alice) - a0, oa);
        assertEq(usdg.balanceOf(carol) - c0, oc);
        assertEq(exchange.unclaimed(bob), ob);
        assertEq(exchange.queued(), ob);
        assertEq(exchange.freeFloat(), 0);
    }

    function test_withdrawUnclaimedGuards() public {
        uint256 a = _mint(alice, ID, 500e6);
        _drainFloat();
        uint256 oa = _redeem(alice, ID, a);
        usdg.freeze(alice);
        usdg.mint(address(exchange), oa);
        exchange.payQueue(1);
        assertEq(exchange.unclaimed(alice), oa);

        // Only to itself, and it is still frozen: the transfer reverts, nothing changes.
        vm.prank(alice);
        vm.expectRevert();
        exchange.withdrawUnclaimed(alice);
        assertEq(exchange.unclaimed(alice), oa);
        // Someone else cannot take it.
        vm.prank(bob);
        vm.expectRevert(PExchange.ZeroAmount.selector);
        exchange.withdrawUnclaimed(bob);
        // Works while the exchange is halted: it is a debt, not a trade.
        vm.prank(keeper);
        exchange.halt();
        usdg.unfreeze(alice);
        uint256 before = usdg.balanceOf(alice);
        vm.prank(alice);
        assertEq(exchange.withdrawUnclaimed(alice), oa);
        assertEq(usdg.balanceOf(alice) - before, oa);
        assertEq(exchange.queued(), 0);
    }

    /**
     * An undelivered claim stays in `queued`, but `_payQueue` measures each
     * later claim against the whole balance, so later sellers can be paid out of
     * USDG that is owed to the earlier, set-aside one. Nothing is lost —
     * `freeFloat` still reserves it and it is paid on the next refill — but the
     * earlier claimant waits behind people who sold after them.
     */
    function test_setAsideClaimKeepsItsUsdgFromLaterSellers() public {
        uint256 a = _mint(alice, ID, 500e6);
        uint256 b = _mint(bob, ID, 500e6);
        _drainFloat();
        uint256 oa = _redeem(alice, ID, a);
        uint256 ob = _redeem(bob, ID, b);
        usdg.freeze(alice);
        // Exactly enough for Alice's claim only.
        usdg.mint(address(exchange), oa);
        exchange.payQueue(10);
        assertEq(exchange.unclaimed(alice), oa);
        // v8: Alice's set-aside USDG stays hers. Bob waits for a refill.
        assertEq(exchange.queueLength(), 1);
        assertEq(usdg.balanceOf(address(exchange)), exchange.unclaimedTotal());
        usdg.unfreeze(alice);
        uint256 aliceBefore = usdg.balanceOf(alice);
        vm.prank(alice);
        exchange.withdrawUnclaimed(alice);
        assertEq(usdg.balanceOf(alice) - aliceBefore, oa);
        uint256 bobBefore = usdg.balanceOf(bob);
        usdg.mint(address(exchange), ob);
        exchange.payQueue(10);
        assertEq(usdg.balanceOf(bob) - bobBefore, ob);
        assertEq(exchange.queued(), 0);
        assertEq(exchange.freeFloat(), usdg.balanceOf(address(exchange)));
    }

    function test_bridgeCanNeverTouchQueuedOrUnclaimed() public {
        uint256 a = _mint(alice, ID, 500e6);
        uint256 b = _mint(bob, ID, 500e6);
        _drainFloat();
        uint256 oa = _redeem(alice, ID, a);
        _redeem(bob, ID, b);
        uint256 ob = exchange.queued() - oa;
        usdg.freeze(alice);
        usdg.mint(address(exchange), oa + ob + 10e6);
        exchange.payQueue(1); // alice set aside, bob still queued
        uint256 free = exchange.freeFloat();
        assertEq(free, 10e6);
        vm.startPrank(keeper);
        vm.expectRevert(abi.encodeWithSelector(PExchange.InsufficientFloat.selector, free + 1, free));
        exchange.sendToBridge(free + 1);
        exchange.sendToBridge(free);
        vm.stopPrank();
        assertGe(usdg.balanceOf(address(exchange)), exchange.queued());
    }

    function test_payQueueRespectsMaxAndIsOpenToAnyone() public {
        uint256[3] memory s;
        address[3] memory who = [alice, bob, carol];
        for (uint256 i; i < 3; ++i) {
            s[i] = _mint(who[i], ID, 100e6);
        }
        _drainFloat();
        for (uint256 i; i < 3; ++i) {
            _redeem(who[i], ID, s[i]);
        }
        usdg.mint(address(exchange), 1_000e6);
        vm.prank(makeAddr("anyone"));
        assertEq(exchange.payQueue(2), 2);
        assertEq(exchange.queueLength(), 1);
        assertEq(exchange.payQueue(0), 0);
        assertEq(exchange.payQueue(5), 1);
        assertEq(exchange.payQueue(5), 0);
    }

    /// v9: a halt freezes queue payments too, until the owner resumes.
    function test_haltStopsTradesAndQueuePayments() public {
        uint256 a = _mint(alice, ID, 100e6);
        _drainFloat();
        uint256 oa = _redeem(alice, ID, a / 2);
        vm.prank(keeper);
        exchange.halt();
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, SELL);
        vm.startPrank(alice);
        vm.expectRevert(PExchange.Halted.selector);
        exchange.redeem(address(pA), 1e6, 0, alice, q, sig);
        (q, sig) = _qNow(ID, BUY);
        vm.expectRevert(PExchange.Halted.selector);
        exchange.mint(address(pA), 1e6, 0, alice, q, sig);
        vm.stopPrank();
        uint256 before = usdg.balanceOf(alice);
        usdg.mint(address(exchange), oa);
        vm.expectRevert(PExchange.Halted.selector);
        exchange.payQueue(1);
        // Keeper cannot resume.
        vm.prank(keeper);
        vm.expectRevert();
        exchange.resume();
        vm.prank(alice);
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.halt();
        vm.prank(owner);
        exchange.resume();
        exchange.payQueue(1);
        assertEq(usdg.balanceOf(alice) - before, oa);
    }

    function test_rescueNeverTakesTheFloatOrShares() public {
        StrayToken stray = new StrayToken();
        stray.transfer(address(exchange), 5e18);
        _mint(alice, ID, 10e6);
        vm.prank(alice);
        pA.transfer(address(exchange), 1e6);
        vm.startPrank(owner);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotRescuable.selector, address(usdg)));
        exchange.rescue(IERC20(address(usdg)), owner, 1);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotRescuable.selector, address(pA)));
        exchange.rescue(IERC20(address(pA)), owner, 1);
        exchange.rescue(IERC20(address(stray)), owner, 5e18);
        vm.stopPrank();
        assertEq(stray.balanceOf(owner), 5e18);
    }

    /// A queue payment's inner transfer runs with 63/64 of the gas; a caller who
    /// under-funds `payQueue` must not be able to push a healthy seller's claim
    /// into `unclaimed` (EIP-150 leaves too little for the bookkeeping after it).
    function test_payQueueGasGriefingCannotSetAsideAHealthyClaim() public {
        uint256 a = _mint(alice, ID, 500e6);
        _drainFloat();
        uint256 oa = _redeem(alice, ID, a);
        usdg.mint(address(exchange), oa);
        for (uint256 g = 25_000; g < 200_000; g += 1_000) {
            uint256 snap = vm.snapshotState();
            (bool ok,) = address(exchange).call{gas: g}(abi.encodeCall(PExchange.payQueue, (1)));
            if (ok) {
                assertEq(exchange.unclaimed(alice), 0, "under-gassed payQueue set a healthy claim aside");
            }
            vm.revertToState(snap);
        }
    }

    function test_nothingToZeroAddressOnAnyPath() public {
        uint256 s = _mint(alice, ID, 100e6);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.startPrank(alice);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.mint(address(pA), 10e6, 0, address(0), q, sig);
        (q, sig) = _qNow(ID, SELL);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.redeem(address(pA), s, 0, address(0), q, sig);
        vm.stopPrank();
        _settle(ID, 1e6);
        PriceOracle.Quote memory junk;
        vm.startPrank(alice);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.mint(address(pA), 10e6, 0, address(0), junk, "");
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.redeem(address(pA), s, 0, address(0), junk, "");
        vm.stopPrank();
    }

    function test_unknownOrForeignTokenIsRefused() public {
        StrayToken stray = new StrayToken();
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, address(stray)));
        exchange.mint(address(stray), 10e6, 0, alice, q, sig);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, address(stray)));
        exchange.absorb(address(stray));
    }

    function test_onlyFactoryOrOwnerCreatesPTokensAndIdempotent() public {
        vm.prank(alice);
        vm.expectRevert(PExchange.OnlyFactory.selector);
        exchange.ensurePToken(0xC0C);
        vm.prank(owner);
        PToken again = exchange.ensurePToken(ID);
        assertEq(address(again), address(pA));
        vm.prank(alice);
        vm.expectRevert();
        pA.mint(alice, 1);
        vm.prank(alice);
        vm.expectRevert();
        pA.burn(bob, 1);
    }

    /* =============================================== 6. band and rounding */

    function test_bandBoundaries() public {
        uint64 lo = exchange.minPrice();
        uint64 hi = exchange.maxPrice();
        assertEq(hi, 980_000, "the default ceiling is 98c");
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, lo, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        (q, sig) = _q(ID, BUY, lo - 1, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PriceOutOfBand.selector, lo - 1));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        (q, sig) = _q(ID, BUY, hi, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        (q, sig) = _q(ID, BUY, hi + 1, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PriceOutOfBand.selector, hi + 1));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);

        // Per-market override up to 99c.
        vm.startPrank(owner);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setMaxPrice(ID, 990_001);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setMaxPrice(ID, lo);
        exchange.setMaxPrice(ID, 990_000);
        vm.stopPrank();
        (q, sig) = _q(ID, BUY, 990_000, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        (q, sig) = _q(ID, BUY, 990_001, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PriceOutOfBand.selector, 990_001));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        // The override does not leak to other markets; 0 resets.
        assertEq(exchange.maxPriceOf(ID_B), hi);
        vm.prank(owner);
        exchange.setMaxPrice(ID, 0);
        assertEq(exchange.maxPriceOf(ID), hi);
    }

    function test_sellsHaveNoBand() public {
        uint256 s = _mint(alice, ID, 100e6);
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, SELL, 999_999, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        exchange.redeem(address(pA), s / 2, 0, alice, q, sig);
        (q, sig) = _q(ID, SELL, 1, 1e12, uint64(block.timestamp + 15));
        vm.prank(alice);
        exchange.redeem(address(pA), s / 2, 0, alice, q, sig);
    }

    function test_paramBounds() public {
        vm.startPrank(owner);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(501, 25, 50_000, 950_000, 2_000e6);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(25, 501, 50_000, 950_000, 2_000e6);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(25, 25, 950_000, 950_000, 2_000e6);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setParams(25, 25, 50_000, 1e6, 2_000e6);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setSettleFee(201);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setOutflowCap(10_000e6, 100_001);
        vm.stopPrank();
    }

    function test_settledAtZeroRefusesMintsAndBurnsForNothing() public {
        uint256 s = _mint(alice, ID, 100e6);
        _settle(ID, 0);
        PriceOracle.Quote memory junk;
        vm.startPrank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.SettledAtZero.selector, ID));
        exchange.mint(address(pA), 10e6, 0, alice, junk, "");
        // minOut protects a holder from burning into nothing.
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, 0, 1));
        exchange.redeem(address(pA), s, 1, alice, junk, "");
        assertEq(exchange.redeem(address(pA), s, 0, alice, junk, ""), 0);
        vm.stopPrank();
        assertEq(pA.balanceOf(alice), 0);
    }

    function test_settledPayoutMath(uint64 payout, uint256 usdgIn) public {
        payout = uint64(bound(payout, 1, 1e6));
        usdgIn = bound(usdgIn, 1e6, 50_000e6);
        vm.prank(owner);
        exchange.setOutflowCap(type(uint256).max, 0);
        _settle(ID, payout);
        PriceOracle.Quote memory junk;
        uint256 denom = (uint256(payout) * (10_000 + exchange.buySpreadBps()) + 9_999) / 10_000;
        uint256 expectOut = (usdgIn * 1e6) / denom;
        vm.assume(expectOut > 0);
        vm.prank(alice);
        uint256 out = exchange.mint(address(pA), usdgIn, 0, alice, junk, "");
        assertEq(out, expectOut);
        vm.prank(alice);
        uint256 back = exchange.redeem(address(pA), out, 0, alice, junk, "");
        assertEq(back, (out * ((uint256(payout) * (10_000 - exchange.settleFeeBps())) / 10_000)) / 1e6);
        assertLe(back, usdgIn, "settled round trip returned more than paid");
    }

    /// Rounding always favours the exchange: shares minted never worth more than
    /// paid at price x (1 + spread); USDG paid never more than shares x price x (1 - spread).
    function test_roundingFavoursTheExchange(uint64 price, uint256 amount, uint16 spread) public view {
        price = uint64(bound(price, 1, 1e6 - 1));
        amount = bound(amount, 0, 1e18);
        spread = uint16(bound(spread, 0, 500));
        uint256 BPS = 10_000;
        // mint side
        uint256 denom = (uint256(price) * (BPS + spread) + BPS - 1) / BPS;
        uint256 out = (amount * 1e6) / denom;
        assertLe(out * uint256(price) * (BPS + spread), amount * 1e6 * BPS, "mint over-issues");
        // redeem side
        uint256 per = (uint256(price) * (BPS - spread)) / BPS;
        uint256 usd = (amount * per) / 1e6;
        assertLe(usd * 1e6 * BPS, amount * uint256(price) * (BPS - spread), "redeem over-pays");
        // and the exchange's own views agree at the configured spreads
        if (spread == exchange.buySpreadBps()) assertEq(exchange.mintOut(price, amount), out);
        if (spread == exchange.sellSpreadBps()) assertEq(exchange.redeemOut(price, amount), usd);
    }

    /// No price and size lets a trader mint then redeem at the same price for a profit.
    function test_roundTripAtOnePriceNeverProfits(uint64 price, uint256 usdgIn) public {
        price = uint64(bound(price, exchange.minPrice(), exchange.maxPrice()));
        usdgIn = bound(usdgIn, 1, 5_000e6);
        vm.prank(owner);
        exchange.setMaxRisk(ID, type(uint128).max);
        (PriceOracle.Quote memory q, bytes memory sig) =
            _q(ID, BUY, price, type(uint256).max, uint64(block.timestamp + 15));
        vm.prank(alice);
        try exchange.mint(address(pA), usdgIn, 0, alice, q, sig) returns (uint256 shares) {
            (q, sig) = _q(ID, SELL, price, type(uint256).max, uint64(block.timestamp + 15));
            vm.prank(alice);
            uint256 back = exchange.redeem(address(pA), shares, 0, alice, q, sig);
            assertLe(back, usdgIn);
        } catch (bytes memory reason) {
            assertEq(bytes4(reason), PExchange.ZeroAmount.selector);
        }
    }

    /// Many dust mints must not beat one big mint (no rounding farming).
    function test_dustMintsNeverBeatOneMint(uint64 price, uint8 n, uint256 each) public {
        price = uint64(bound(price, exchange.minPrice(), exchange.maxPrice()));
        n = uint8(bound(n, 2, 40));
        each = bound(each, 1, 50e6);
        uint256 many;
        for (uint256 i; i < n; ++i) {
            many += exchange.mintOut(price, each);
        }
        assertLe(many, exchange.mintOut(price, each * n));
    }

    function test_slippageGuardsEveryPath() public {
        uint256 expect = exchange.mintOut(600_000, 100e6);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.startPrank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, expect, expect + 1));
        exchange.mint(address(pA), 100e6, expect + 1, alice, q, sig);
        uint256 s = exchange.mint(address(pA), 100e6, expect, alice, q, sig);
        (q, sig) = _qNow(ID, SELL);
        uint256 rOut = exchange.redeemOut(600_000, s);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, rOut, rOut + 1));
        exchange.redeem(address(pA), s, rOut + 1, alice, q, sig);
        vm.stopPrank();
    }
}
