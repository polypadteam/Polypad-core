// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {PolypadBase} from "../Polypad.t.sol";

/// @dev A USDG with ERC777-style receive hooks: it calls `onUsdgReceived` on a
///      contract recipient, bubbling a revert with data, ignoring a missing hook.
contract CallbackUSDG is ERC20 {
    constructor() ERC20("Hooked Dollar", "USDG") {}

    function decimals() public pure override returns (uint8) {
        return 6;
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (to.code.length > 0 && from != address(0)) {
            (bool ok, bytes memory ret) = to.call(abi.encodeWithSignature("onUsdgReceived(address,uint256)", from, value));
            if (!ok && ret.length > 0) {
                assembly {
                    revert(add(ret, 32), mload(ret))
                }
            }
        }
    }
}

/// @dev Receives USDG and, from inside the transfer, tries one call back into the exchange.
contract Reenterer {
    enum Action {
        None,
        Redeem,
        Mint,
        PayQueue,
        WithdrawUnclaimed,
        Absorb,
        RevertAlways
    }

    PExchange public ex;
    PToken public p;
    Action public action;
    bool public attempted;
    bool public blocked;
    bytes public reason;
    PriceOracle.Quote internal q;
    bytes internal sig;

    constructor(PExchange ex_, PToken p_) {
        ex = ex_;
        p = p_;
    }

    function arm(Action a, PriceOracle.Quote calldata q_, bytes calldata sig_) external {
        action = a;
        q = q_;
        sig = sig_;
        attempted = false;
        blocked = false;
    }

    function redeem(uint256 amount, PriceOracle.Quote calldata q_, bytes calldata sig_) external returns (uint256) {
        return ex.redeem(address(p), amount, 0, address(this), q_, sig_);
    }

    function withdrawUnclaimed(address to) external {
        ex.withdrawUnclaimed(to);
    }

    function onUsdgReceived(address, uint256) external {
        if (action == Action.RevertAlways) revert("no thanks");
        if (action == Action.None || attempted) return;
        attempted = true;
        bool ok;
        bytes memory ret;
        if (action == Action.Redeem) {
            (ok, ret) = address(ex).call(abi.encodeCall(ex.redeem, (address(p), 1e6, 0, address(this), q, sig)));
        } else if (action == Action.Mint) {
            IERC20(address(ex.usdg())).approve(address(ex), type(uint256).max);
            (ok, ret) = address(ex).call(abi.encodeCall(ex.mint, (address(p), 1e6, 0, address(this), q, sig)));
        } else if (action == Action.PayQueue) {
            (ok, ret) = address(ex).call(abi.encodeCall(ex.payQueue, (10)));
        } else if (action == Action.WithdrawUnclaimed) {
            (ok, ret) = address(ex).call(abi.encodeCall(ex.withdrawUnclaimed, (address(this))));
        } else if (action == Action.Absorb) {
            (ok, ret) = address(ex).call(abi.encodeCall(ex.absorb, (address(p))));
        }
        blocked = !ok;
        reason = ret;
    }
}

/**
 * Reentrancy through a hooked collateral token, plus hostile inputs: forged and
 * malleable signatures, quotes for the wrong market or side, unknown pTokens,
 * zero and extreme amounts, labels at their limits.
 */
contract AdversarialReentrancyTest is PolypadBase {
    CallbackUSDG internal cb;
    PExchange internal ex;
    PToken internal cp;
    Reenterer internal r;

    function setUp() public override {
        super.setUp();
        cb = new CallbackUSDG();
        ex = new PExchange(owner, IERC20(address(cb)), oracle, keeper);
        vm.startPrank(owner);
        ex.setRoles(keeper, address(factory), bridge);
        cp = ex.ensurePToken(ID);
        ex.setMaxUnbacked(ID, type(uint256).max);
        vm.stopPrank();
        r = new Reenterer(ex, cp);
        cb.mint(address(ex), 100_000e6);
        cb.mint(address(r), 10_000e6);
        cb.mint(alice, 100_000e6);
        // The attacker holds shares to redeem.
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        cb.approve(address(ex), type(uint256).max);
        ex.mint(address(cp), 5_000e6, 0, address(r), q, sig);
        vm.stopPrank();
    }

    function _reentrancyBlocked() internal view {
        assertTrue(r.attempted(), "hook ran");
        assertTrue(r.blocked(), "re-entry went through");
        assertEq(bytes4(r.reason()), ReentrancyGuard.ReentrancyGuardReentrantCall.selector);
    }

    /* ------------------------------------------------------------ reentrancy */

    function test_reenteringRedeemFromARedeemPayoutIsBlocked() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        r.arm(Reenterer.Action.Redeem, q, sig);
        r.redeem(1_000e6, q, sig);
        _reentrancyBlocked();
    }

    function test_reenteringMintFromARedeemPayoutIsBlocked() public {
        (PriceOracle.Quote memory b, bytes memory bs) = signedQuote(ID, BUY);
        r.arm(Reenterer.Action.Mint, b, bs);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        r.redeem(1_000e6, q, sig);
        _reentrancyBlocked();
    }

    function test_reenteringPayQueueAndWithdrawFromAQueuePayoutIsBlocked() public {
        // Empty the float so the attacker's redemption queues.
        uint256 free = ex.freeFloat();
        vm.prank(keeper);
        ex.sendToBridge(free);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 owed = r.redeem(1_000e6, q, sig);
        assertEq(ex.queued(), owed);

        r.arm(Reenterer.Action.PayQueue, q, sig);
        cb.mint(address(ex), owed);
        ex.payQueue(10);
        _reentrancyBlocked();
        assertEq(ex.queued(), 0);
        assertEq(ex.queueLength(), 0);
    }

    function test_aRecipientThatRejectsIsSetAsideAndTheQueueMovesOn() public {
        uint256 free = ex.freeFloat();
        vm.prank(keeper);
        ex.sendToBridge(free);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 owedR = r.redeem(1_000e6, q, sig);
        // Alice queues behind the attacker.
        (PriceOracle.Quote memory b, bytes memory bs) = signedQuote(ID, BUY);
        vm.prank(alice);
        uint256 shares = ex.mint(address(cp), 100e6, 0, alice, b, bs);
        free = ex.freeFloat();
        vm.prank(keeper);
        ex.sendToBridge(free);
        vm.prank(alice);
        uint256 owedA = ex.redeem(address(cp), shares, 0, alice, q, sig);

        r.arm(Reenterer.Action.RevertAlways, q, sig);
        cb.mint(address(ex), owedR + owedA);
        uint256 aliceBefore = cb.balanceOf(alice);
        ex.payQueue(10);
        assertEq(cb.balanceOf(alice) - aliceBefore, owedA, "alice paid");
        assertEq(ex.unclaimed(address(r)), owedR);
        assertEq(ex.queued(), owedR, "still owed, still reserved");
        assertEq(ex.freeFloat(), cb.balanceOf(address(ex)) - owedR);

        // It can take it later, to another address.
        r.arm(Reenterer.Action.None, q, sig);
        address safe = makeAddr("safe");
        r.withdrawUnclaimed(safe);
        assertEq(cb.balanceOf(safe), owedR);
        assertEq(ex.queued(), 0);
    }

    function test_reenteringWithdrawUnclaimedIsBlocked() public {
        uint256 free = ex.freeFloat();
        vm.prank(keeper);
        ex.sendToBridge(free);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 owed = r.redeem(1_000e6, q, sig);
        r.arm(Reenterer.Action.RevertAlways, q, sig);
        cb.mint(address(ex), owed);
        ex.payQueue(10);
        assertEq(ex.unclaimed(address(r)), owed);

        // Withdraw to itself and try to withdraw again from inside the payout.
        r.arm(Reenterer.Action.WithdrawUnclaimed, q, sig);
        r.withdrawUnclaimed(address(r));
        _reentrancyBlocked();
        assertEq(ex.unclaimed(address(r)), 0);
        assertEq(ex.queued(), 0);
    }

    function test_absorbFromInsideAPayoutIsHarmless() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        r.arm(Reenterer.Action.Absorb, q, sig);
        uint256 supply = cp.totalSupply();
        r.redeem(1_000e6, q, sig);
        assertTrue(r.attempted());
        assertFalse(r.blocked(), "absorb is open, and has nothing to burn");
        assertEq(cp.totalSupply(), supply - 1_000e6);
    }

    /* ------------------------------------------------------------ signatures */

    function test_highSSignatureIsRefused() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        bytes32 r_;
        bytes32 s;
        uint8 v;
        assembly {
            r_ := mload(add(sig, 32))
            s := mload(add(sig, 64))
            v := byte(0, mload(add(sig, 96)))
        }
        uint256 n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;
        bytes32 s2 = bytes32(n - uint256(s));
        bytes memory flipped = abi.encodePacked(r_, s2, v == 27 ? uint8(28) : uint8(27));
        vm.expectRevert();
        oracle.verify(q, flipped, BUY);
        // The original still works.
        assertEq(oracle.verify(q, sig, BUY), q.price);
    }

    function test_compactAndGarbageSignaturesAreRefused() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        bytes memory short_ = new bytes(64);
        for (uint256 i; i < 64; ++i) {
            short_[i] = sig[i];
        }
        vm.expectRevert();
        oracle.verify(q, short_, BUY);
        vm.expectRevert();
        oracle.verify(q, "", BUY);
        vm.expectRevert();
        oracle.verify(q, new bytes(65), BUY);
    }

    function test_aQuoteSignedByAnyoneElseIsRefused() public {
        PriceOracle.Quote memory q = PriceOracle.Quote(ID, BUY, 600_000, type(uint256).max, uint64(block.timestamp + 10));
        bytes memory sig = _sign(q, 0xBAD);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        oracle.verify(q, sig, BUY);
    }

    function test_aQuoteCannotBeAlteredAfterSigning() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        q.price = 50_000;
        vm.expectRevert(PriceOracle.BadSignature.selector);
        oracle.verify(q, sig, BUY);
    }

    function test_aQuoteFromAnotherOracleIsRefused() public {
        PriceOracle other = new PriceOracle(owner, signer, keeper);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        other.verify(q, sig, BUY);
    }

    /* ---------------------------------------------------------------- quotes */

    function test_quoteForOtherMarketSideOrSizeIsRefused() public {
        vm.prank(owner);
        PToken p = exchange.ensurePToken(ID);
        (PriceOracle.Quote memory qb, bytes memory sb) = signedQuote(ID_B, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), type(uint256).max);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteForOtherMarket.selector, ID_B, ID));
        exchange.mint(address(p), 10e6, 0, alice, qb, sb);
        (PriceOracle.Quote memory qs, bytes memory ss) = signedQuote(ID, SELL);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.WrongSide.selector, SELL));
        exchange.mint(address(p), 10e6, 0, alice, qs, ss);
        PriceOracle.Quote memory small = PriceOracle.Quote(ID, BUY, 600_000, 5e6, uint64(block.timestamp + 10));
        bytes memory smallSig = _sign(small, signerPk);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteTooSmall.selector, 10e6, 5e6));
        exchange.mint(address(p), 10e6, 0, alice, small, smallSig);
        vm.stopPrank();
    }

    function test_quoteValidityBoundaries() public {
        uint64 now_ = uint64(block.timestamp);
        PriceOracle.Quote memory q = PriceOracle.Quote(ID, BUY, 600_000, type(uint256).max, now_ + 30);
        assertEq(oracle.verify(q, _sign(q, signerPk), BUY), 600_000, "exactly MAX_VALIDITY ahead");
        q.validUntil = now_ + 31;
        bytes memory sig = _sign(q, signerPk);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteTooLong.selector, now_ + 31));
        oracle.verify(q, sig, BUY);
        q.validUntil = now_;
        assertEq(oracle.verify(q, _sign(q, signerPk), BUY), 600_000, "valid through its last second");
        q.validUntil = now_ - 1;
        sig = _sign(q, signerPk);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteExpired.selector, now_ - 1));
        oracle.verify(q, sig, BUY);
    }

    function test_quotePriceMustBeStrictlyBetweenZeroAndOne() public {
        PriceOracle.Quote memory q = PriceOracle.Quote(ID, BUY, 0, type(uint256).max, uint64(block.timestamp + 5));
        bytes memory sig = _sign(q, signerPk);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID, uint64(0)));
        oracle.verify(q, sig, BUY);
        q.price = 1e6;
        sig = _sign(q, signerPk);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.PriceOutOfRange.selector, ID, uint64(1e6)));
        oracle.verify(q, sig, BUY);
    }

    /// Documented: a quote carries no nonce or taker, so within its <= 30s it can be
    /// reused by anyone, any number of times, each trade up to maxAmount.
    function test_aQuoteIsReusableWithinItsWindow() public {
        vm.prank(owner);
        PToken p = exchange.ensurePToken(ID);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        usdg.approve(address(exchange), type(uint256).max);
        vm.prank(bob);
        usdg.approve(address(exchange), type(uint256).max);
        vm.prank(alice);
        uint256 a = exchange.mint(address(p), 100e6, 0, alice, q, sig);
        vm.prank(bob);
        uint256 b = exchange.mint(address(p), 100e6, 0, bob, q, sig);
        assertEq(a, b);
    }

    /* ---------------------------------------------------------- bad pTokens */

    function test_unknownAndLookalikePTokensAreRefused() public {
        vm.prank(owner);
        PToken real = exchange.ensurePToken(ID);
        // Same market, but minted by our other exchange (the hooked one).
        PToken twin = cp;
        assertEq(twin.positionId(), real.positionId());
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), type(uint256).max);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, address(twin)));
        exchange.mint(address(twin), 10e6, 0, alice, q, sig);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, address(usdg)));
        exchange.mint(address(usdg), 10e6, 0, alice, q, sig);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, alice));
        exchange.redeem(alice, 10e6, 0, alice, q, sig);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, address(twin)));
        exchange.mintPosted(address(twin), 10e6, 0, alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, address(twin)));
        exchange.absorb(address(twin));
        vm.stopPrank();
    }

    /* ---------------------------------------------------- zero and extremes */

    function test_zeroAmountsEverywhere() public {
        vm.prank(owner);
        PToken p = exchange.ensurePToken(ID);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        vm.expectRevert(PExchange.ZeroAmount.selector);
        exchange.mint(address(p), 0, 0, alice, q, sig);
        vm.expectRevert(PExchange.ZeroAmount.selector);
        exchange.redeem(address(p), 0, 0, alice, q, sig);
        vm.expectRevert(PExchange.ZeroAmount.selector);
        exchange.mintPosted(address(p), 0, 0, alice);
        vm.expectRevert(PExchange.ZeroAmount.selector);
        exchange.redeemPosted(address(p), 0, 0, alice);
        vm.stopPrank();
        assertEq(exchange.payQueue(0), 0);
    }

    function test_dustMintRoundsToZeroAndReverts() public {
        vm.prank(owner);
        PToken p = exchange.ensurePToken(ID);
        // 950000 x 1.0025 rounds up: 1 unit of USDG buys 1e6/952375 = 1 share unit.
        // At 95c even one unit buys a unit; nothing mints for free.
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), 1);
        uint256 out = exchange.mint(address(p), 1, 0, alice, q, sig);
        vm.stopPrank();
        assertLe(out, 1);
    }

    function test_redeemingDustPaysNothingAndBurnsNothingForFree() public {
        vm.prank(owner);
        PToken p = exchange.ensurePToken(ID);
        (PriceOracle.Quote memory b, bytes memory bs) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), 10e6);
        exchange.mint(address(p), 10e6, 0, alice, b, bs);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 before = usdg.balanceOf(alice);
        // 1 unit x 0.5985 floors to 0: burned, nothing paid, no claim queued.
        uint256 out = exchange.redeem(address(p), 1, 0, alice, q, sig);
        vm.stopPrank();
        assertEq(out, 0);
        assertEq(usdg.balanceOf(alice), before);
        assertEq(exchange.queueLength(), 0);
    }

    /// A claim is stored as uint96 while `queued` keeps the full amount: above
    /// 2^96 units (7.9e22 USDG) the claim is silently truncated and the difference
    /// is reserved forever. Unreachable with real USDG supply, but a SafeCast is free.
    function test_queuedClaimAboveUint96RevertsInsteadOfTruncating() public {
        vm.startPrank(owner);
        PToken p = exchange.ensurePToken(ID);
        exchange.setMaxUnbacked(ID, type(uint256).max);
        exchange.setOutflowCap(type(uint256).max);
        vm.stopPrank();
        uint256 big = uint256(type(uint96).max) * 2;
        usdg.mint(alice, big);
        (PriceOracle.Quote memory b, bytes memory bs) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), big);
        uint256 shares = exchange.mint(address(p), big, 0, alice, b, bs);
        vm.stopPrank();
        uint256 free = exchange.freeFloat();
        vm.prank(keeper);
        exchange.sendToBridge(free);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 queuedBefore = exchange.queued();
        uint256 lengthBefore = exchange.queueLength();
        uint256 owed = exchange.redeemOut(q.price, shares);
        assertGt(owed, type(uint96).max);
        // Regression: a claim that does not fit its uint96 slot reverts the whole
        // redemption (SafeCast), instead of queuing a truncated claim while
        // `queued` reserves the full amount.
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSignature("SafeCastOverflowedUintDowncast(uint8,uint256)", uint8(96), owed));
        exchange.redeem(address(p), shares, 0, alice, q, sig);
        assertEq(exchange.queued(), queuedBefore);
        assertEq(exchange.queueLength(), lengthBefore);
        assertEq(p.balanceOf(alice), shares, "nothing burned");

        // A claim that fits is queued exactly.
        uint256 part = shares / 4;
        vm.prank(alice);
        uint256 got = exchange.redeem(address(p), part, 0, alice, q, sig);
        (, uint96 amount) = exchange.claims(exchange.claimHead());
        assertEq(uint256(amount), got);
        assertEq(exchange.queued(), queuedBefore + got);
    }



    /* ---------------------------------------------------------------- labels */

    function test_shareLabelLimitsAreBytes() public {
        vm.prank(owner);
        PToken p = exchange.ensurePToken(ID);
        string memory n64 = "0123456789012345678901234567890123456789012345678901234567890123";
        string memory n65 = string.concat(n64, "4");
        vm.startPrank(keeper);
        exchange.setShareLabel(ID, n64, "0123456789abcdef");
        assertEq(p.name(), n64);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setShareLabel(ID, n65, "p");
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setShareLabel(ID, "x", "0123456789abcdefg");
        // Six 3-byte characters: 18 bytes, too long for a symbol.
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setShareLabel(ID, "x", unicode"日本日本日本");
        exchange.setShareLabel(ID, unicode"Ja, Österreich", unicode"pÖST");
        assertEq(p.symbol(), unicode"pÖST");
        // Empty resets to the defaults.
        exchange.setShareLabel(ID, "", "");
        assertEq(p.name(), "Polypad Share");
        assertEq(p.symbol(), "pSHARE");
        // No pToken yet for this market.
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.setShareLabel(12345, "x", "y");
        vm.stopPrank();
    }
}
