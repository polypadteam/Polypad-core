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
    address internal poster = makeAddr("poster");
    address internal carol = makeAddr("carol");
    PToken internal pA;
    PToken internal pB;

    function setUp() public override {
        super.setUp();
        vm.startPrank(owner);
        oracle.setPoster(poster);
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
        // Enough backing that the unbacked cap is out of the way unless a test wants it.
        _back(ID, 1_000_000e6);
        _back(ID_B, 1_000_000e6);
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

    function _postOnChain(uint256 id, uint64 price) internal {
        uint256[] memory ids = new uint256[](1);
        uint64[] memory prices = new uint64[](1);
        ids[0] = id;
        prices[0] = price;
        vm.prank(poster);
        oracle.post(ids, prices);
    }

    function _alive() internal {
        vm.prank(poster);
        oracle.alive();
    }

    function _expire(uint256 id) internal {
        uint256[] memory ids = new uint256[](1);
        ids[0] = id;
        vm.prank(poster);
        oracle.expire(ids);
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
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, 600_000, 50e6, uint64(block.timestamp + 15));
        vm.prank(alice);
        exchange.mint(address(pA), 50e6, 0, alice, q, sig);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteTooSmall.selector, 50e6 + 1, 50e6));
        exchange.mint(address(pA), 50e6 + 1, 0, alice, q, sig);
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
        oracle.setPoster(alice);
        vm.expectRevert();
        oracle.setKeeper(alice);
        vm.expectRevert();
        oracle.setPostParams(900, 90, 30_000, 15);
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.setPaused(new uint256[](0), true);
        vm.expectRevert(PriceOracle.OnlyPoster.selector);
        oracle.alive();
        vm.expectRevert(PriceOracle.OnlyPoster.selector);
        oracle.post(new uint256[](0), new uint64[](0));
        vm.stopPrank();
    }

    /**
     * Regression: a changed poster must wait ROLE_DELAY, so a stolen owner key
     * cannot start posting prices at once. Revoking is immediate, and a set after
     * a revoke no longer counts as the first set: it waits like any change.
     */
    function test_posterDelayNotBypassedByRevokeThenSet() public {
        address evil = makeAddr("evil");
        vm.startPrank(owner);
        oracle.setPoster(address(0)); // revoke: immediate, fine
        oracle.setPoster(evil); // waits ROLE_DELAY like any change
        vm.stopPrank();
        assertEq(oracle.poster(), address(0), "a new poster took effect without the delay");
        assertEq(oracle.pendingPoster(), evil);
        // With no poster the posted path is closed: nobody can post or ping.
        vm.prank(evil);
        vm.expectRevert(PriceOracle.OnlyPoster.selector);
        oracle.alive();
    }

    /// Documented behaviour: a quote is a price, not a ticket. It may be used any
    /// number of times until it expires, each use bounded by `maxAmount`; the
    /// unbacked cap and the outflow cap bound the total.
    function test_aQuoteReplaysWithinItsLifeEachUseBoundedByMaxAmount() public {
        _back(ID, 0);
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, 600_000, 300e6, uint64(block.timestamp + 15));
        uint256 total;
        vm.startPrank(alice);
        for (uint256 i; i < 4; ++i) {
            total += exchange.mint(address(pA), 300e6, 0, alice, q, sig);
        }
        // 1,200 USDG through a 300 USDG quote, all in one block...
        assertGt(total, 1_900e6);
        // ...until the unbacked cap (2,000 shares) stops it.
        vm.expectRevert();
        exchange.mint(address(pA), 300e6, 0, alice, q, sig);
        vm.stopPrank();
        // Anyone may use anyone's quote: it binds no taker.
        vm.prank(bob);
        vm.expectRevert(); // still the cap, not the signature
        exchange.mint(address(pA), 300e6, 0, bob, q, sig);
    }

    /* =============================================== 2. posted path */

    function test_postedPriceUsableThroughMaxPostAgeNotAfter() public {
        uint256 t0 = block.timestamp;
        uint256 age = oracle.maxPostAge();
        for (uint256 t = 60; t < age; t += 60) {
            vm.warp(t0 + t);
            _alive();
        }
        vm.warp(t0 + age);
        _alive();
        assertEq(oracle.postedPrice(ID, BUY), 600_000);
        vm.warp(t0 + age + 1);
        _alive();
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PostStale.selector, ID, uint64(t0)));
        oracle.postedPrice(ID, BUY);
    }

    function test_posterSilenceClosesEveryMarketAtTheBoundary() public {
        uint64 aliveAt = oracle.posterAliveAt();
        vm.warp(aliveAt + oracle.maxPosterSilence());
        assertEq(oracle.postedPrice(ID, SELL), 600_000);
        assertEq(oracle.postedPrice(ID_B, SELL), 300_000);
        vm.warp(aliveAt + oracle.maxPosterSilence() + 1);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PosterSilent.selector, aliveAt));
        oracle.postedPrice(ID, SELL);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PosterSilent.selector, aliveAt));
        oracle.postedPrice(ID_B, SELL);
        _alive();
        assertEq(oracle.postedPrice(ID, SELL), 600_000);
    }

    function test_neverPostedMarketHasNoPostedPrice() public {
        vm.prank(owner);
        exchange.ensurePToken(0xC0C);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.NoPostedPrice.selector, 0xC0C));
        oracle.postedPrice(0xC0C, BUY);
    }

    function test_jumpHaltBoundaries() public {
        uint64 jump = oracle.postJumpAbs();
        // One unit under the jump: no halt.
        _postOnChain(ID, 600_000 + jump - 1);
        assertEq(oracle.postedPrice(ID, BUY), 600_000 + jump - 1);
        // Exactly the jump (measured from the last post): halted for postCooldown.
        _postOnChain(ID, 600_000 + 2 * jump - 1);
        (,, uint64 haltedUntil) = oracle.posted(ID);
        assertEq(haltedUntil, block.timestamp + oracle.postCooldown());
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PostHalted.selector, ID, haltedUntil));
        oracle.postedPrice(ID, SELL);
        vm.warp(haltedUntil - 1);
        _alive();
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PostHalted.selector, ID, haltedUntil));
        oracle.postedPrice(ID, SELL);
        vm.warp(haltedUntil);
        _alive();
        assertEq(oracle.postedPrice(ID, SELL), 600_000 + 2 * jump - 1);
        // A jump on one market leaves the others alone.
        assertEq(oracle.postedPrice(ID_B, SELL), 300_000);
    }

    function test_jumpDownHaltsToo() public {
        _postOnChain(ID, 600_000 - oracle.postJumpAbs());
        vm.expectRevert();
        oracle.postedPrice(ID, BUY);
    }

    function test_postOutsideZeroOneIsRefusedWhole() public {
        uint256[] memory ids = new uint256[](2);
        uint64[] memory prices = new uint64[](2);
        ids[0] = ID;
        ids[1] = ID_B;
        prices[0] = 610_000;
        prices[1] = 1e6;
        vm.prank(poster);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID_B, 1e6));
        oracle.post(ids, prices);
        (uint64 price,,) = oracle.posted(ID);
        assertEq(price, 600_000, "the whole batch reverted");
        prices = new uint64[](1);
        vm.prank(poster);
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.post(ids, prices);
    }

    function test_expireOnlyByThePosterAndOnlyLivePosts() public {
        uint256[] memory ids = new uint256[](2);
        ids[0] = ID;
        ids[1] = 0xC0C; // never posted
        vm.prank(alice);
        vm.expectRevert(PriceOracle.OnlyPoster.selector);
        oracle.expire(ids);

        vm.recordLogs();
        vm.prank(poster);
        oracle.expire(ids);
        assertEq(vm.getRecordedLogs().length, 1, "one PostExpired, none for the never-posted id");
        (, uint64 atNever,) = oracle.posted(0xC0C);
        assertEq(atNever, 0, "never-posted stays never-posted");
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.NoPostedPrice.selector, 0xC0C));
        oracle.postedPrice(0xC0C, BUY);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PostStale.selector, ID, 1));
        oracle.postedPrice(ID, BUY);

        // Expiring again is a no-op.
        vm.recordLogs();
        vm.prank(poster);
        oracle.expire(ids);
        assertEq(vm.getRecordedLogs().length, 0);
    }

    function test_expiredMarketReopensOnTheNextPostAndStillJumpChecks() public {
        _expire(ID);
        // Same price: reopens at once.
        _postOnChain(ID, 600_000);
        assertEq(oracle.postedPrice(ID, BUY), 600_000);
        // Expire, then repost far away: the jump is measured from the kept price.
        _expire(ID);
        _postOnChain(ID, 700_000);
        vm.expectRevert();
        oracle.postedPrice(ID, BUY);
    }

    function test_expiredMarketRefusesPostedTradesButNotSignedOnes() public {
        uint256 shares = _mint(alice, ID, 100e6);
        _expire(ID);
        vm.startPrank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PostStale.selector, ID, 1));
        exchange.mintPosted(address(pA), 10e6, 0, alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PostStale.selector, ID, 1));
        exchange.redeemPosted(address(pA), shares, 0, alice);
        vm.stopPrank();
        assertGt(_redeem(alice, ID, shares), 0);
    }

    function test_pausedMarketClosesPostedBothWaysSignedSellsOnly() public {
        uint256 shares = _mint(alice, ID, 100e6);
        uint256[] memory ids = new uint256[](1);
        ids[0] = ID;
        vm.prank(keeper);
        oracle.setPaused(ids, true);
        vm.startPrank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        exchange.mintPosted(address(pA), 10e6, 0, alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        exchange.redeemPosted(address(pA), shares, 0, alice);
        vm.stopPrank();
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        exchange.mint(address(pA), 10e6, 0, alice, q, sig);
        assertGt(_redeem(alice, ID, shares / 2), 0);
        // Unpause reopens everything.
        vm.prank(keeper);
        oracle.setPaused(ids, false);
        vm.prank(alice);
        exchange.redeemPosted(address(pA), shares / 4, 0, alice);
    }

    function test_settledMarketTradesAtPayoutOnBothPathsQuotesIgnored() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _settle(ID, 1e6);
        // The settle delay aged the post out; a fresh one isolates the settled check.
        _postOnChain(ID, 600_000);
        // postedPrice itself refuses a settled market...
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketSettled.selector, ID));
        oracle.postedPrice(ID, SELL);
        // ...but the exchange routes settled redemptions to the payout.
        uint256 before = usdg.balanceOf(alice);
        vm.prank(alice);
        uint256 out = exchange.redeemPosted(address(pA), shares / 2, 0, alice);
        assertEq(out, ((shares / 2) * ((1e6 * uint256(10_000 - exchange.settleFeeBps())) / 10_000)) / 1e6);
        assertEq(usdg.balanceOf(alice) - before, out);
        // Signed redeem with a garbage quote: ignored, payout used.
        PriceOracle.Quote memory junk;
        vm.prank(alice);
        uint256 out2 = exchange.redeem(address(pA), shares / 2, 0, alice, junk, "");
        assertEq(out2, out);
        // Settled mints skip the posted caps and the quote.
        vm.prank(alice);
        exchange.mintPosted(address(pA), 5_000e6, 0, alice);
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

    function test_postedTradeCapIsInclusive() public {
        uint256 max = exchange.postedMaxTrade();
        vm.startPrank(alice);
        exchange.mintPosted(address(pA), max, 0, alice);
        vm.roll(block.number + 1);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PostedTradeTooLarge.selector, max + 1, max));
        exchange.mintPosted(address(pA), max + 1, 0, alice);
        vm.stopPrank();
    }

    function test_postedRedeemCapIsMeasuredInUsdgOut() public {
        uint256 shares = _mint(alice, ID, 5_000e6);
        uint256 max = exchange.postedMaxTrade();
        // Shares worth just over the cap at the posted sell price.
        uint256 sellPx = (uint256(600_000) * (10_000 - exchange.postedSpreadBps())) / 10_000;
        uint256 over = ((max + 1) * 1e6 + sellPx - 1) / sellPx;
        assertLt(over, shares);
        vm.prank(alice);
        vm.expectRevert(); // PostedTradeTooLarge
        exchange.redeemPosted(address(pA), over, 0, alice);
        vm.prank(alice);
        exchange.redeemPosted(address(pA), over - 2, 0, alice);
    }

    function test_postedBlockCapCountsMintsAndRedeemsTogetherPerMarket() public {
        uint256 shares = _mint(alice, ID, 3_000e6);
        uint256 perBlock = exchange.postedMaxPerBlock();
        vm.roll(block.number + 1);
        vm.startPrank(alice);
        exchange.mintPosted(address(pA), 500e6, 0, alice);
        exchange.mintPosted(address(pA), 500e6, 0, alice);
        uint256 out = exchange.redeemPosted(address(pA), (shares * 400) / 3_000, 0, alice);
        uint256 used = 1_000e6 + out;
        uint256 left = perBlock - used;
        exchange.mintPosted(address(pA), left / 2, 0, alice);
        exchange.mintPosted(address(pA), left - left / 2, 0, alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PostedBlockCap.selector, perBlock + 1, perBlock));
        exchange.mintPosted(address(pA), 1, 0, alice);
        // A sell counts against the same bucket.
        vm.expectRevert();
        exchange.redeemPosted(address(pA), 10e6, 0, alice);
        // Another market has its own bucket.
        exchange.mintPosted(address(pB), 500e6, 0, alice);
        vm.stopPrank();
        // Next block: fresh bucket.
        vm.roll(block.number + 1);
        vm.prank(alice);
        exchange.mintPosted(address(pA), 500e6, 0, alice);
    }

    function test_postedBlockCapIsPerBlockNotPerTimestamp() public {
        vm.startPrank(alice);
        for (uint256 i; i < 4; ++i) {
            exchange.mintPosted(address(pA), 500e6, 0, alice);
        }
        // Time moves, block does not: still capped.
        vm.warp(block.timestamp + 60);
        vm.stopPrank();
        _alive();
        vm.prank(alice);
        vm.expectRevert();
        exchange.mintPosted(address(pA), 1e6, 0, alice);
    }

    function test_postedMintRespectsTheBand() public {
        _postOnChain(ID_B, 300_000 - 29_000);
        vm.warp(block.timestamp + 1);
        _alive();
        // Walk ID_B's post down under 5c in sub-jump steps.
        uint64 p = 271_000;
        while (p > 60_000) {
            p -= 29_000;
            _postOnChain(ID_B, p);
        }
        _postOnChain(ID_B, 49_999);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PriceOutOfBand.selector, 49_999));
        exchange.mintPosted(address(pB), 10e6, 0, alice);
    }

    /**
     * KNOWN, and why the posted path ships closed (postedMaxTrade = 0). Its
     * defence against the lag between a Polymarket move and the next post is the
     * jump halt. But the halt starts only once the new post lands, and blocks
     * trading only during the cooldown: a trader who bought at the stale price
     * before the post simply waits the cooldown out and sells at the new one.
     * Here, with the path opened at its old settings: 60c -> 70c, $2,000 in (one
     * block's cap), ~13% profit out of the float. Only a two-step fill closes it.
     */
    function test_KNOWN_postedLatencyArbProfitsAcrossAJump() public {
        uint256 before = usdg.balanceOf(alice);
        // Polymarket has already moved to 70c; the keeper's post is in flight.
        vm.startPrank(alice);
        uint256 shares;
        for (uint256 i; i < 4; ++i) {
            shares += exchange.mintPosted(address(pA), 500e6, 0, alice);
        }
        vm.stopPrank();
        _postOnChain(ID, 700_000); // halts 15s
        vm.warp(block.timestamp + oracle.postCooldown());
        vm.roll(block.number + 1);
        _alive();
        vm.startPrank(alice);
        uint256 chunk = shares / 5;
        for (uint256 i; i < 5; ++i) {
            if (i == 4) chunk = pA.balanceOf(alice);
            exchange.redeemPosted(address(pA), chunk, 0, alice);
            vm.roll(block.number + 1);
        }
        vm.stopPrank();
        assertGt(usdg.balanceOf(alice), before, "the leak the closed default guards against");
    }

    /// KNOWN, same leak without a jump: at low prices a sub-halt move is many
    /// times the 1.5% posted spread. 10c -> 12.9c is under the 3c halt, so not
    /// even halted. Another reason the posted path ships closed.
    function test_KNOWN_postedLatencyArbAtLowPricesUnderTheJumpThreshold() public {
        vm.prank(owner);
        exchange.setMaxUnbacked(ID_B, 1_000_000e6);
        // Walk ID_B to 10c in sub-jump steps.
        uint64[7] memory path = [uint64(271_000), 242_000, 213_000, 184_000, 155_000, 126_000, 100_000];
        for (uint256 i; i < path.length; ++i) {
            _postOnChain(ID_B, path[i]);
        }
        uint256 before = usdg.balanceOf(alice);
        vm.prank(alice);
        uint256 shares = exchange.mintPosted(address(pB), 500e6, 0, alice);
        _postOnChain(ID_B, 129_000); // +2.9c: 29% move, no halt
        vm.roll(block.number + 1);
        vm.startPrank(alice);
        exchange.redeemPosted(address(pB), shares / 2, 0, alice);
        vm.roll(block.number + 1);
        exchange.redeemPosted(address(pB), pB.balanceOf(alice), 0, alice);
        vm.stopPrank();
        assertGt(usdg.balanceOf(alice), before, "the leak the closed default guards against");
    }

    /// With the shipped settings (postedMaxTrade = postedMaxPerBlock = 0) neither
    /// leg of the latency arb is possible: every posted mint and redeem reverts,
    /// while signed quotes and settled payouts work as before.
    function test_postedLatencyArbImpossibleWithTheDefaultConfig() public {
        vm.prank(owner);
        exchange.setPostedParams(150, 0, 0);
        uint256 shares = _mint(alice, ID, 1_000e6);
        vm.startPrank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PostedTradeTooLarge.selector, 500e6, 0));
        exchange.mintPosted(address(pA), 500e6, 0, alice);
        uint256 out = exchange.redeemPostedOut(ID, shares);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PostedTradeTooLarge.selector, out, 0));
        exchange.redeemPosted(address(pA), shares, 0, alice);
        vm.stopPrank();
        _postOnChain(ID, 700_000);
        vm.warp(block.timestamp + oracle.postCooldown());
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PostedTradeTooLarge.selector, 1e6, 0));
        exchange.mintPosted(address(pA), 1e6, 0, alice);
        assertGt(_redeem(alice, ID, shares), 0);
    }

    /* =============================================== 3. outflow cap */

    function _toLastSecondOfHour() internal {
        uint256 t = (block.timestamp / 3_600 + 1) * 3_600 - 1;
        vm.warp(t);
        _alive();
    }

    function test_outflowCapExactFillThenHourRollover() public {
        uint256 shares = _mint(alice, ID, 10_000e6);
        uint256 amount = shares / 4;
        uint256 out = exchange.redeemOut(600_000, amount);
        vm.prank(owner);
        exchange.setOutflowCap(out * 2);
        _toLastSecondOfHour();
        _redeem(alice, ID, amount);
        _redeem(alice, ID, amount); // exactly fills the hour
        assertEq(exchange.outflowRemaining(), 0);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, SELL);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.OutflowCap.selector, exchange.redeemOut(600_000, 1e6), 0));
        exchange.redeem(address(pA), 1e6, 0, alice, q, sig);
        // One second later is a new clock hour.
        vm.warp(block.timestamp + 1);
        assertEq(exchange.outflowRemaining(), out * 2);
        _redeem(alice, ID, amount);
    }

    function test_outflowCapCountsSignedPostedSettledAndQueuedAlike() public {
        uint256 shares = _mint(alice, ID, 4_000e6);
        uint256 sharesB = _mint(alice, ID_B, 1_000e6);
        vm.prank(owner);
        exchange.setOutflowCap(1_000_000e6);
        uint256 used0 = 1_000_000e6 - exchange.outflowRemaining();
        uint256 a = _redeem(alice, ID, shares / 4);
        vm.prank(alice);
        uint256 b = exchange.redeemPosted(address(pA), shares / 8, 0, alice);
        _drainFloat();
        uint256 c = _redeem(alice, ID, shares / 4); // queued entirely
        assertEq(exchange.queued(), c);
        assertEq(1_000_000e6 - exchange.outflowRemaining() - used0, a + b + c);
        // A settlement takes SETTLE_DELAY, which rolls into a later clock hour:
        // the settled redemption is the only outflow of that hour.
        _settle(ID_B, 1e6);
        assertEq(exchange.outflowRemaining(), 1_000_000e6);
        vm.prank(alice);
        uint256 d = exchange.redeemPosted(address(pB), sharesB, 0, alice);
        assertEq(1_000_000e6 - exchange.outflowRemaining(), d);
    }

    function test_loweringTheCapBelowUsedClosesTheHourWithoutUnderflow() public {
        uint256 shares = _mint(alice, ID, 1_000e6);
        _redeem(alice, ID, shares / 2);
        vm.prank(owner);
        exchange.setOutflowCap(1);
        assertEq(exchange.outflowRemaining(), 0);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, SELL);
        vm.prank(alice);
        vm.expectRevert();
        exchange.redeem(address(pA), 1e6, 0, alice, q, sig);
    }

    /* =============================================== 4. unbacked cap */

    function test_unbackedCapExactBoundary() public {
        _back(ID, 0);
        // At 60c + 0.25% the share costs 0.6015: 1,203 USDG buys exactly 2,000 shares.
        assertEq(exchange.mintOut(600_000, 1_203e6), 2_000e6);
        _mint(alice, ID, 1_203e6);
        assertEq(pA.totalSupply(), exchange.defaultMaxUnbacked());
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        uint256 more = exchange.mintOut(600_000, 1e6);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnbackedCap.selector, 2_000e6 + more, 2_000e6));
        exchange.mint(address(pA), 1e6, 0, alice, q, sig);
    }

    function test_backingMovesTheCapAndSellsStillWorkWhenOverIt() public {
        _back(ID, 0);
        uint256 shares = _mint(alice, ID, 1_203e6);
        _back(ID, 1_000e6);
        uint256 more = _mint(alice, ID, 601_500_000); // exactly 1,000 more shares
        assertEq(more, 1_000e6);
        // Desk reports less than it had: supply is now well over backing + cap.
        _back(ID, 0);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(alice);
        vm.expectRevert();
        exchange.mint(address(pA), 1e6, 0, alice, q, sig);
        // Selling never depends on backing.
        assertGt(_redeem(alice, ID, shares), 0);
    }

    function test_perMarketUnbackedOverrideAndItsZeroMeansDefault() public {
        _back(ID, 0);
        vm.prank(owner);
        exchange.setMaxUnbacked(ID, 100e6);
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, BUY);
        vm.prank(alice);
        vm.expectRevert();
        exchange.mint(address(pA), 100e6, 0, alice, q, sig);
        _mint(alice, ID, 60e6);
        // Setting 0 does NOT close the market: it restores the 2,000 default.
        // (To stop mints on one market use pause, or a 1-wei cap.)
        vm.prank(owner);
        exchange.setMaxUnbacked(ID, 0);
        assertEq(exchange.maxUnbacked(ID), exchange.defaultMaxUnbacked());
        _mint(alice, ID, 600e6);
    }

    function test_settledMintsBypassTheUnbackedCap() public {
        vm.prank(owner);
        exchange.setOutflowCap(1_000_000e6);
        _back(ID, 0);
        _settle(ID, 1e6);
        PriceOracle.Quote memory junk;
        vm.prank(alice);
        uint256 out = exchange.mint(address(pA), 50_000e6, 0, alice, junk, "");
        assertGt(out, exchange.defaultMaxUnbacked());
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

        vm.startPrank(alice);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.withdrawUnclaimed(address(0));
        // To itself (still frozen): the transfer reverts and nothing changes.
        vm.expectRevert();
        exchange.withdrawUnclaimed(alice);
        vm.stopPrank();
        assertEq(exchange.unclaimed(alice), oa);
        // Someone else cannot take it.
        vm.prank(bob);
        vm.expectRevert(PExchange.ZeroAmount.selector);
        exchange.withdrawUnclaimed(bob);
        // Works while the exchange is halted: it is a debt, not a trade.
        vm.prank(keeper);
        exchange.halt();
        address fresh = makeAddr("fresh");
        vm.prank(alice);
        assertEq(exchange.withdrawUnclaimed(fresh), oa);
        assertEq(usdg.balanceOf(fresh), oa);
        assertEq(exchange.queued(), 0);
    }

    /**
     * An undelivered claim stays in `queued`, but `_payQueue` measures each
     * later claim against the whole balance, so later sellers can be paid out of
     * USDG that is owed to the earlier, set-aside one. Nothing is lost —
     * `freeFloat` still reserves it and it is paid on the next refill — but the
     * earlier claimant waits behind people who sold after them.
     */
    function test_setAsideClaimCanWaitBehindLaterSellersUntilRefill() public {
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
        if (ob <= oa) {
            // Bob was paid from the USDG set aside for Alice.
            assertEq(exchange.queueLength(), 0);
            assertLt(usdg.balanceOf(address(exchange)), exchange.queued());
            vm.prank(alice);
            vm.expectRevert();
            exchange.withdrawUnclaimed(makeAddr("fresh"));
            usdg.mint(address(exchange), ob);
        }
        vm.prank(alice);
        exchange.withdrawUnclaimed(makeAddr("fresh"));
        assertEq(exchange.freeFloat(), usdg.balanceOf(address(exchange)) - exchange.queued());
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

    function test_haltStopsTradesNotQueuePayments() public {
        uint256 a = _mint(alice, ID, 100e6);
        _drainFloat();
        uint256 oa = _redeem(alice, ID, a / 2);
        vm.prank(keeper);
        exchange.halt();
        (PriceOracle.Quote memory q, bytes memory sig) = _qNow(ID, SELL);
        vm.startPrank(alice);
        vm.expectRevert(PExchange.Halted.selector);
        exchange.redeem(address(pA), 1e6, 0, alice, q, sig);
        vm.expectRevert(PExchange.Halted.selector);
        exchange.redeemPosted(address(pA), 1e6, 0, alice);
        vm.expectRevert(PExchange.Halted.selector);
        exchange.mintPosted(address(pA), 1e6, 0, alice);
        vm.stopPrank();
        uint256 before = usdg.balanceOf(alice);
        usdg.mint(address(exchange), oa);
        exchange.payQueue(1);
        assertEq(usdg.balanceOf(alice) - before, oa);
        // Keeper cannot resume.
        vm.prank(keeper);
        vm.expectRevert();
        exchange.resume();
        vm.prank(alice);
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.halt();
        vm.prank(owner);
        exchange.resume();
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
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.mintPosted(address(pA), 10e6, 0, address(0));
        (q, sig) = _qNow(ID, SELL);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.redeem(address(pA), s, 0, address(0), q, sig);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.redeemPosted(address(pA), s, 0, address(0));
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
        exchange.setPostedParams(1_001, 500e6, 2_000e6);
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.setPostParams(0, 90, 30_000, 15);
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.setPostParams(86_401, 90, 30_000, 15);
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.setPostParams(900, 601, 30_000, 15);
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.setPostParams(900, 90, 0, 15);
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.setPostParams(900, 90, 30_000, 3_601);
        vm.stopPrank();
    }

    function test_settledAtZeroRefusesMintsAndBurnsForNothing() public {
        uint256 s = _mint(alice, ID, 100e6);
        _settle(ID, 0);
        PriceOracle.Quote memory junk;
        vm.startPrank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.SettledAtZero.selector, ID));
        exchange.mint(address(pA), 10e6, 0, alice, junk, "");
        vm.expectRevert(abi.encodeWithSelector(PExchange.SettledAtZero.selector, ID));
        exchange.mintPosted(address(pA), 10e6, 0, alice);
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
        exchange.setOutflowCap(type(uint256).max);
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
        exchange.setMaxUnbacked(ID, type(uint128).max);
        (PriceOracle.Quote memory q, bytes memory sig) = _q(ID, BUY, price, type(uint256).max, uint64(block.timestamp + 15));
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
        uint256 postedExpect = exchange.mintPostedOut(ID, 100e6);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, postedExpect, postedExpect + 1));
        exchange.mintPosted(address(pA), 100e6, postedExpect + 1, alice);
        (q, sig) = _qNow(ID, SELL);
        uint256 rOut = exchange.redeemOut(600_000, s);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, rOut, rOut + 1));
        exchange.redeem(address(pA), s, rOut + 1, alice, q, sig);
        uint256 pOut = exchange.redeemPostedOut(ID, s);
        vm.expectRevert(abi.encodeWithSelector(PExchange.Slippage.selector, pOut, pOut + 1));
        exchange.redeemPosted(address(pA), s, pOut + 1, alice);
        vm.stopPrank();
    }

    /// Price moving between quote fetch and inclusion: the signed quote pins the
    /// price, so the user's minOut is met or the tx reverts; a posted trade sees
    /// the new post and minOut catches it.
    function test_postMovesBetweenQuoteAndInclusionMinOutCatchesIt() public {
        uint256 expect = exchange.mintPostedOut(ID, 100e6);
        _postOnChain(ID, 620_000); // lands first
        vm.prank(alice);
        vm.expectRevert();
        exchange.mintPosted(address(pA), 100e6, expect, alice);
    }
}
