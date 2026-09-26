// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

import {BondingCurve} from "../src/BondingCurve.sol";
import {Coin} from "../src/Coin.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {PToken} from "../src/PToken.sol";
import {Router} from "../src/Router.sol";
import {PolypadBase, MockUSDG} from "./Polypad.t.sol";

/// @dev Random buys, sells and price moves from several traders.
contract CurveHandler is Test {
    Router internal router;
    BondingCurve internal curve;
    Coin internal coin;
    MockUSDG internal usdg;
    PolypadBase internal base;
    address[] internal traders;

    uint256 public usdgIn;
    uint256 public usdgOut;

    constructor(PolypadBase base_, Router router_, BondingCurve curve_, MockUSDG usdg_, address[] memory traders_) {
        base = base_;
        router = router_;
        curve = curve_;
        coin = Coin(address(curve_.coin()));
        usdg = usdg_;
        traders = traders_;
    }

    function buy(uint256 who, uint256 amount) external {
        address t = traders[who % traders.length];
        amount = bound(amount, 1e6, 5_000e6);
        if (curve.soldOut()) return;
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 0);
        vm.startPrank(t);
        uint256 before = usdg.balanceOf(t);
        try router.buy(curve, amount, 0, t, q, sig) {
            usdgIn += before - usdg.balanceOf(t);
        } catch {}
        vm.stopPrank();
    }

    function sell(uint256 who, uint256 fraction) external {
        address t = traders[who % traders.length];
        uint256 bal = coin.balanceOf(t);
        if (bal == 0) return;
        uint256 amount = (bal * bound(fraction, 1, 100)) / 100;
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 1);
        vm.startPrank(t);
        coin.approve(address(router), amount);
        uint256 before = usdg.balanceOf(t);
        try router.sell(curve, amount, 0, t, q, sig) {
            usdgOut += usdg.balanceOf(t) - before;
        } catch {}
        vm.stopPrank();
    }
}

contract CurveInvariants is PolypadBase {
    CurveHandler internal handler;
    BondingCurve internal curve;
    Coin internal coin;
    PToken internal p;

    function setUp() public override {
        super.setUp();
        (coin, curve) = _launch(ID);
        p = exchange.pTokenOf(ID);
        vm.prank(owner);
        exchange.setMaxUnbacked(ID, type(uint256).max);

        address[] memory traders = new address[](3);
        for (uint256 i; i < 3; ++i) {
            traders[i] = makeAddr(string(abi.encodePacked("trader", i)));
            usdg.mint(traders[i], 1_000_000e6);
            vm.prank(traders[i]);
            usdg.approve(address(router), type(uint256).max);
        }
        handler = new CurveHandler(this, router, curve, usdg, traders);
        targetContract(address(handler));
    }

    /// The curve always holds the pToken it thinks it holds.
    function invariant_curveHoldsTrackedQuote() public view {
        assertEq(p.balanceOf(address(curve)), curve.trackedQuote());
    }

    /// The curve always holds exactly the coins it tracks.
    function invariant_curveHoldsTrackedTokens() public view {
        assertEq(coin.balanceOf(address(curve)), curve.trackedTokens());
    }

    /// Selling every outstanding coin can never need more pToken than the curve holds.
    function invariant_curveIsSolvent() public view {
        uint256 outstanding = coin.SUPPLY() - curve.trackedTokens();
        uint256 gross = (outstanding * (curve.phantom() + curve.trackedQuote())) / (curve.trackedTokens() + outstanding);
        assertLe(gross, curve.trackedQuote() + 1);
    }

    /// Never sells past the reserve.
    function invariant_reserveUntouched() public view {
        assertGe(curve.trackedTokens(), curve.reserved());
    }

    /// Traders as a group never take out more USDG than they put in (price fixed here).
    function invariant_tradersNeverProfitAtFixedPrice() public view {
        assertLe(handler.usdgOut(), handler.usdgIn());
    }
}
