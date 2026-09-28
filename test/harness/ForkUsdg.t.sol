// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";

import {ForkBase, IUSDG} from "./ForkBase.t.sol";

/// @dev How real USDG's zero-address rule, freezes and pause meet the exchange.
contract ForkUsdgTest is ForkBase {
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;
    IUSDG internal u = IUSDG(USDG);
    address internal protector = makeAddr("protector");

    function setUp() public override {
        super.setUp();
        (coin, curve) = _launch(ID, 0);
        p = PToken(exchange.pTokenAddress(ID));
        _usdgRole("ASSET_PROTECTION_ROLE", protector);
        _usdgRole("PAUSE_ROLE", protector);
    }

    /// @dev Three sellers queued behind an empty float, oldest first: alice, bob, carol.
    function _queueThree() internal returns (uint256[3] memory owed) {
        (uint256 a,) = _buy(alice, curve, 500e6);
        (uint256 b,) = _buy(bob, curve, 400e6);
        (uint256 c,) = _buy(carol, curve, 300e6);
        uint256 all = usdg.balanceOf(address(exchange));
        vm.prank(keeper);
        exchange.sendToBridge(all);
        owed[0] = _sell(alice, curve, a);
        owed[1] = _sell(bob, curve, b);
        owed[2] = _sell(carol, curve, c);
        assertEq(exchange.queueLength(), 3);
    }

    function test_realUsdgRefusesTheZeroAddressAndSoDoesTheExchange() public {
        vm.prank(alice);
        vm.expectRevert(bytes4(0xd92e233d)); // USDG ZeroAddress()
        usdg.transfer(address(0), 1);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), 10e6);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.mint(address(p), 10e6, 0, address(0), q, sig);
        uint256 shares = exchange.mint(address(p), 10e6, 0, alice, q, sig);
        (q, sig) = signedQuote(ID, SELL);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.redeem(address(p), shares, 0, address(0), q, sig);
        vm.stopPrank();
    }

    function test_aFrozenSellerIsSetAsideAndTheQueueMovesOn() public {
        uint256[3] memory owed = _queueThree();
        vm.prank(protector);
        u.freeze(alice);
        assertTrue(u.isFrozen(alice));

        _fund(address(exchange), owed[0] + owed[1] + owed[2]);
        uint256 bobBefore = usdg.balanceOf(bob);
        uint256 carolBefore = usdg.balanceOf(carol);
        exchange.payQueue(10);
        assertEq(usdg.balanceOf(bob) - bobBefore, owed[1]);
        assertEq(usdg.balanceOf(carol) - carolBefore, owed[2]);
        assertEq(exchange.queueLength(), 0);
        assertEq(exchange.unclaimed(alice), owed[0]);
        assertEq(exchange.queued(), owed[0], "still owed, still reserved");
        assertEq(exchange.freeFloat(), 0);

        // A frozen address cannot route its claim around the freeze: it is paid
        // only to itself, which USDG refuses while frozen.
        vm.prank(alice);
        vm.expectRevert();
        exchange.withdrawUnclaimed(alice);
        assertEq(exchange.unclaimed(alice), owed[0]);
        // Once Paxos lifts the freeze, it takes it.
        vm.prank(protector);
        u.unfreeze(alice);
        uint256 before = usdg.balanceOf(alice);
        vm.prank(alice);
        exchange.withdrawUnclaimed(alice);
        assertEq(usdg.balanceOf(alice) - before, owed[0]);
        assertEq(exchange.queued(), 0);
    }

    function test_aFrozenSellerCanNotBeServedDirectlyEither() public {
        (uint256 a,) = _buy(alice, curve, 500e6);
        vm.prank(protector);
        u.freeze(alice);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.startPrank(alice);
        coin.approve(address(router), a);
        vm.expectRevert(); // the float would pay now: USDG refuses, the whole sell reverts
        router.sell(curve, a, 0, alice, q, sig);
        // To another address it works.
        router.sell(curve, a, 0, bob, q, sig);
        vm.stopPrank();
    }

    /// @dev If USDG ever freezes the exchange itself, every USDG path stops; nothing is lost.
    function test_aFrozenExchangeStopsEveryUsdgPath() public {
        (uint256 a,) = _buy(alice, curve, 500e6);
        vm.prank(protector);
        u.freeze(address(exchange));
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(bob);
        vm.expectRevert();
        router.buy(curve, 100e6, 0, bob, q, sig);
        (q, sig) = signedQuote(ID, SELL);
        vm.startPrank(alice);
        coin.approve(address(router), a);
        vm.expectRevert();
        router.sell(curve, a, 0, alice, q, sig);
        vm.stopPrank();
    }

    /**
     * @dev Regression. USDG paused: a transfer fails for EVERY recipient, not
     *      because of the recipient. `payQueue` must not read that as "this
     *      recipient is bad" and set the whole queue aside; claims stay in line
     *      until USDG resumes.
     */
    function test_usdgPauseKeepsTheQueueInLine() public {
        uint256[3] memory owed = _queueThree();
        _fund(address(exchange), owed[0] + owed[1] + owed[2]);
        vm.prank(protector);
        u.pause();
        assertTrue(u.paused());

        exchange.payQueue(10); // anyone can call this

        assertEq(exchange.queueLength(), 3, "claims left the queue during a token-wide pause");
        assertEq(exchange.unclaimed(alice) + exchange.unclaimed(bob) + exchange.unclaimed(carol), 0);
    }

    /// @dev A pause is visible to a contract: even a zero transfer to itself fails
    ///      (the basis for telling "token paused" from "recipient frozen").
    function test_aPauseFailsAZeroSelfTransferAFreezeDoesNot() public {
        vm.prank(protector);
        u.freeze(alice);
        vm.prank(address(exchange));
        assertTrue(usdg.transfer(address(exchange), 0));
        vm.prank(protector);
        u.pause();
        vm.prank(address(exchange));
        vm.expectRevert();
        usdg.transfer(address(exchange), 0);
    }

    /// @dev During a pause every USDG-moving call reverts; it resumes cleanly.
    function test_usdgPauseBlocksBuysAndSells() public {
        (uint256 a,) = _buy(alice, curve, 500e6);
        vm.prank(protector);
        u.pause();
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(bob);
        vm.expectRevert();
        router.buy(curve, 100e6, 0, bob, q, sig);

        (q, sig) = signedQuote(ID, SELL);
        vm.startPrank(alice);
        coin.approve(address(router), a);
        vm.expectRevert();
        router.sell(curve, a / 2, 0, alice, q, sig);
        vm.stopPrank();

        vm.prank(protector);
        u.unpause();
        assertGt(_sell(alice, curve, a / 2), 0);
    }
}
