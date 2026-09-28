// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {PExchange} from "../src/PExchange.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {PToken} from "../src/PToken.sol";
import {MockUSDG, PolypadBase} from "./Polypad.t.sol";

/// @dev What the owner can and cannot do, and how fast.
contract GovernanceTest is PolypadBase {
    address internal newSigner = makeAddr("newSigner");

    function test_aNewSignerWaitsTwoDays() public {
        vm.prank(owner);
        oracle.setSigner(newSigner);
        assertEq(oracle.signer(), signer, "old signer still in charge");

        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.NotYet.selector, block.timestamp + 2 days));
        oracle.acceptSigner();

        vm.warp(block.timestamp + 2 days);
        vm.prank(owner);
        oracle.acceptSigner();
        assertEq(oracle.signer(), newSigner);
    }

    function test_revokingTheSignerIsImmediate() public {
        vm.prank(owner);
        oracle.setSigner(address(0));
        assertEq(oracle.signer(), address(0));
        // No quote can verify now: the signed path is closed.
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        oracle.verify(q, sig, BUY);
    }

    function test_theBridgeDepositChangesOnlyWithDelay() public {
        address attacker = makeAddr("attacker");
        vm.prank(owner);
        vm.expectRevert(PExchange.BridgeChangeDelayed.selector);
        exchange.setRoles(keeper, address(factory), attacker);

        vm.prank(owner);
        exchange.proposeBridgeDeposit(attacker);
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotYet.selector, block.timestamp + 2 days));
        exchange.acceptBridgeDeposit();
        assertEq(exchange.bridgeDeposit(), bridge);

        vm.warp(block.timestamp + 2 days);
        vm.prank(owner);
        exchange.acceptBridgeDeposit();
        assertEq(exchange.bridgeDeposit(), attacker);
    }

    function test_rescueNeverTouchesTheFloatOrPTokens() public {
        _launch(ID);
        address p = address(exchange.pTokenOf(ID));
        vm.startPrank(owner);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotRescuable.selector, address(usdg)));
        exchange.rescue(IERC20(address(usdg)), owner, 1);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotRescuable.selector, p));
        exchange.rescue(IERC20(p), owner, 1);
        vm.stopPrank();

        MockUSDG stray = new MockUSDG();
        stray.mint(address(exchange), 5);
        vm.prank(owner);
        exchange.rescue(IERC20(address(stray)), alice, 5);
        assertEq(stray.balanceOf(alice), 5);
    }

    function test_ownershipMovesInTwoSteps() public {
        address next = makeAddr("next");
        vm.prank(owner);
        exchange.transferOwnership(next);
        assertEq(exchange.owner(), owner, "nothing moves until accepted");
        vm.prank(next);
        exchange.acceptOwnership();
        assertEq(exchange.owner(), next);
    }

    function test_keeperNamesShareTokensAfterTheirOutcome() public {
        _launch(ID);
        PToken p = exchange.pTokenOf(ID);
        assertEq(p.name(), "Polypad Share");
        assertEq(p.symbol(), "pSHARE");
        address at = exchange.pTokenAddress(ID);

        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.setShareLabel(ID, "fake", "FAKE");
        vm.expectRevert(PToken.OnlyExchange.selector);
        p.setLabel("fake", "FAKE");
        vm.expectRevert(PExchange.BadParams.selector);
        vm.prank(keeper);
        exchange.setShareLabel(ID_B, "no token yet", "X");

        vm.prank(keeper);
        exchange.setShareLabel(ID, "Polypad YES \u00b7 Fed holds rates", "pYES-FEDHOLD");
        assertEq(p.name(), "Polypad YES \u00b7 Fed holds rates");
        assertEq(p.symbol(), "pYES-FEDHOLD");
        assertEq(address(p), at); // the address does not depend on the name
        vm.prank(owner);
        exchange.setShareLabel(ID, "Polypad YES \u00b7 Fed holds", "pYES-FED");
        assertEq(p.symbol(), "pYES-FED");
    }
}
