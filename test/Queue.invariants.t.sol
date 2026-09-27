// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

import {BondingCurve} from "../src/BondingCurve.sol";
import {Coin} from "../src/Coin.sol";
import {PExchange} from "../src/PExchange.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {Router} from "../src/Router.sol";
import {MockUSDG, PolypadBase} from "./Polypad.t.sol";

/// @dev Random buys, sells, float sent to the desk, float coming back and queue payments.
contract QueueHandler is Test {
    PolypadBase internal base;
    Router internal router;
    PExchange internal exchange;
    BondingCurve internal curve;
    Coin internal coin;
    MockUSDG internal usdg;
    address internal keeper;
    address[] internal traders;

    /// USDG the sellers were promised in total, and what reached them.
    uint256 public promised;
    uint256 public received;

    constructor(
        PolypadBase base_,
        Router router_,
        PExchange exchange_,
        BondingCurve curve_,
        MockUSDG usdg_,
        address keeper_,
        address[] memory traders_
    ) {
        base = base_;
        router = router_;
        exchange = exchange_;
        curve = curve_;
        coin = Coin(address(curve_.coin()));
        usdg = usdg_;
        keeper = keeper_;
        traders = traders_;
    }

    function buy(uint256 who, uint256 amount) external {
        address t = traders[who % traders.length];
        amount = bound(amount, 1e6, 3_000e6);
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 0);
        uint256 before = usdg.balanceOf(t);
        vm.prank(t);
        try router.buy(curve, amount, 0, t, q, sig) {
            // A mint can pay queued sellers, this trader included.
            uint256 spent = before + amount - usdg.balanceOf(t);
            received += amount - spent;
        } catch {}
    }

    function sell(uint256 who, uint256 fraction) external {
        address t = traders[who % traders.length];
        uint256 bal = coin.balanceOf(t);
        if (bal == 0) return;
        uint256 amount = (bal * bound(fraction, 1, 100)) / 100;
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 1);
        uint256 before = usdg.balanceOf(t);
        vm.startPrank(t);
        coin.approve(address(router), amount);
        try router.sell(curve, amount, 0, t, q, sig) returns (uint256 out) {
            promised += out;
            received += usdg.balanceOf(t) - before;
        } catch {}
        vm.stopPrank();
    }

    function toDesk(uint256 fraction) external {
        uint256 amount = (exchange.freeFloat() * bound(fraction, 0, 100)) / 100;
        vm.prank(keeper);
        exchange.sendToBridge(amount);
    }

    function fromDesk(uint256 amount) external {
        usdg.mint(address(exchange), bound(amount, 0, 5_000e6));
    }

    function pay(uint256 max) external {
        uint256[] memory before = new uint256[](traders.length);
        for (uint256 i; i < traders.length; ++i) {
            before[i] = usdg.balanceOf(traders[i]);
        }
        exchange.payQueue(bound(max, 0, 20));
        for (uint256 i; i < traders.length; ++i) {
            received += usdg.balanceOf(traders[i]) - before[i];
        }
    }
}

contract QueueInvariants is PolypadBase {
    QueueHandler internal handler;
    BondingCurve internal curve;

    function setUp() public override {
        super.setUp();
        (, curve) = _launch(ID);
        vm.startPrank(owner);
        exchange.setMaxUnbacked(ID, type(uint256).max);
        exchange.setOutflowCap(type(uint256).max);
        vm.stopPrank();
        // Start from an empty float: every sell leans on the queue.
        uint256 seed = usdg.balanceOf(address(exchange));
        vm.prank(keeper);
        exchange.sendToBridge(seed);

        address[] memory traders = new address[](3);
        for (uint256 i; i < 3; ++i) {
            traders[i] = makeAddr(string(abi.encodePacked("q", i)));
            usdg.mint(traders[i], 1_000_000e6);
            vm.prank(traders[i]);
            usdg.approve(address(router), type(uint256).max);
        }
        handler = new QueueHandler(this, router, exchange, curve, usdg, keeper, traders);
        targetContract(address(handler));
    }

    /// `queued` is exactly what the unpaid claims add up to.
    function invariant_queuedMatchesClaims() public view {
        uint256 sum;
        for (uint256 i = exchange.claimHead(); i < exchange.queueLength() + exchange.claimHead(); ++i) {
            (, uint96 amount) = exchange.claims(i);
            sum += amount;
        }
        assertEq(sum, exchange.queued());
    }

    /// Every seller is paid or owed exactly what they were promised, never more.
    function invariant_promisedIsPaidOrOwed() public view {
        assertEq(handler.promised(), handler.received() + exchange.queued());
    }

    /// The keeper can never send USDG owed to the queue to the desk.
    function invariant_bridgeNeverTouchesOwed() public view {
        if (exchange.queued() > 0) {
            assertEq(
                exchange.freeFloat(),
                usdg.balanceOf(address(exchange)) > exchange.queued()
                    ? usdg.balanceOf(address(exchange)) - exchange.queued()
                    : 0
            );
        }
    }
}
