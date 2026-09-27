// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {PToken} from "../../src/PToken.sol";

import {CurveHarnessBase} from "./CurveHarnessBase.sol";

/// @dev Rounding: dust trades, split trades and arbitrary buy/sell cycles never let
///      a trader take out more pToken than they put in.
contract CurveRoundingTest is CurveHarnessBase {
    uint256 internal constant BPS = 10_000;

    function test_sellingOneWeiOfCoinRevertsNotPays() public {
        (uint256 coins,) = _curveBuy(alice, _shares(alice, 100e6), 0);
        assertGt(coins, 1);
        assertEq(curve.quoteSell(1), 0);
        vm.startPrank(alice);
        coin.approve(address(curve), 1);
        vm.expectRevert(BondingCurve.ZeroAmount.selector);
        curve.sell(1, 0, alice);
        vm.stopPrank();
    }

    /// Fees round up, so one unit of pToken is all fee (ceil(1 x 140 / 10_000) = 1):
    /// nothing is left to buy with, and the buy reverts instead of minting coins
    /// for free. Two units buy coins, and those coins sell back for at most that.
    function test_oneWeiOfPTokenBuyRevertsAndTwoRoundTripForAtMost() public {
        _shares(alice, 1e6);
        uint256 p0 = p.balanceOf(alice);
        vm.startPrank(alice);
        p.approve(address(curve), 1);
        vm.expectRevert(BondingCurve.ZeroAmount.selector);
        curve.buy(1, 0, alice);
        vm.stopPrank();
        assertEq(p.balanceOf(alice), p0);
        (uint256 coins,) = _curveBuy(alice, 2, 0);
        assertGt(coins, 0);
        if (curve.quoteSell(coins) > 0) _curveSell(alice, coins, 0);
        assertLe(p.balanceOf(alice), p0);
        _assertCurveSolvent();
    }

    /// Fees round up, so no trade is too small to pay one: a dust buy pays at least
    /// one unit of fee, and the fee is never more than one unit over the exact rate.
    function test_dustTradesPayAtLeastOneUnitOfFee() public {
        uint256 dust = BPS / curve.feeBps(); // 71 at 1.4%
        _shares(alice, 1e6);
        uint256 plat0 = p.balanceOf(platform);
        uint256 vault0 = p.balanceOf(address(vault));
        _curveBuy(alice, dust, 0);
        uint256 paid = (p.balanceOf(platform) - plat0) + (p.balanceOf(address(vault)) - vault0);
        assertGe(paid, 1, "dust must pay a fee");
        assertLe(paid, (dust * curve.feeBps()) / BPS + 1, "at most one unit over the exact rate");
    }

    /// Splitting a buy into N parts never yields more coins than one buy of the whole,
    /// beyond the N units of pToken fee rounding can save.
    function testFuzz_splitBuysNeverBeatOneBuy(uint256 total, uint256 n) public {
        total = bound(total, 1_000, 5_000e6);
        n = bound(n, 2, 25);
        _shares(alice, 5_000e6);
        uint256 v = curve.phantom() + curve.trackedQuote();
        uint256 t = curve.trackedTokens();
        uint256 one = curve.quoteBuy(total);

        uint256 got;
        uint256 part = total / n;
        vm.assume(part > 0);
        for (uint256 i; i < n; ++i) {
            uint256 amt = i == n - 1 ? total - part * (n - 1) : part;
            if (curve.quoteBuy(amt) == 0) continue;
            (uint256 o,) = _curveBuy(alice, amt, 0);
            got += o;
        }
        // Each saved unit of pToken is worth at most t / v coins at the start price.
        assertLe(got, one + n * (t / v + 1), "splitting beat one buy by more than fee rounding");
    }

    /// Any sequence of one trader's buys and sells, ending with selling everything,
    /// never returns more pToken than it started with.
    function testFuzz_singleTraderCyclesNeverProfit(uint256[12] memory ops) public {
        uint256 start = _shares(attacker, 6_000e6);
        for (uint256 i; i < ops.length; ++i) {
            uint256 op = ops[i];
            uint256 have = p.balanceOf(attacker);
            uint256 coins = coin.balanceOf(attacker);
            if (op % 2 == 0) {
                uint256 amt = op % 3 == 0 ? bound(op >> 8, 1, 100) : bound(op >> 8, 1, have / 4 + 1);
                if (amt > have || curve.quoteBuy(amt) == 0) continue;
                if (curve.quoteBuy(amt) >= curve.trackedTokens() - curve.reserved()) continue; // stay on the curve
                _curveBuy(attacker, amt, 0);
            } else if (coins > 0) {
                uint256 amt = op % 3 == 0 ? bound(op >> 8, 1, coins) : (coins * bound(op >> 8, 1, 100)) / 100;
                if (amt == 0 || curve.quoteSell(amt) == 0) continue;
                _curveSell(attacker, amt, 0);
            }
            _assertCurveSolvent();
        }
        uint256 left = coin.balanceOf(attacker);
        if (left > 0 && curve.quoteSell(left) > 0) _curveSell(attacker, left, 0);
        assertLe(p.balanceOf(attacker), start, "a buy/sell cycle made pToken");
    }

    /// Tokens sent straight to the curve are ignored: price and payouts are unchanged,
    /// and the donation just sits there.
    function test_donationsDoNotMoveTheCurve() public {
        (uint256 coins,) = _curveBuy(alice, _shares(alice, 1_000e6), 0);
        uint256 quoteBefore = curve.quoteSell(coins / 2);
        uint256 donation = _shares(bob, 500e6);
        vm.prank(bob);
        p.transfer(address(curve), donation);
        vm.prank(alice);
        coin.transfer(address(curve), coins / 4);
        assertEq(curve.quoteSell(coins / 2), quoteBefore);
        assertEq(p.balanceOf(address(curve)), curve.trackedQuote() + donation);
    }
}

/// @dev Random dust and normal trades from several traders plus donations, on the curve.
contract CurveDustHandler is Test {
    BondingCurve internal curve;
    Coin internal coin;
    PToken internal p;
    address[] internal traders;
    address internal immutable single;

    constructor(BondingCurve curve_, address[] memory traders_) {
        curve = curve_;
        coin = Coin(address(curve_.coin()));
        p = PToken(address(curve_.pToken()));
        traders = traders_;
        single = traders_[0];
    }

    function buy(uint256 who, uint256 amt, bool dust) external {
        address t = traders[who % traders.length];
        uint256 have = p.balanceOf(t);
        if (have == 0) return;
        amt = dust ? bound(amt, 1, 200) : bound(amt, 1, have / 3 + 1);
        if (amt > have) return;
        // Keep the curve open so the invariants stay about the curve.
        if (curve.quoteBuy(amt) >= curve.trackedTokens() - curve.reserved()) return;
        vm.startPrank(t);
        p.approve(address(curve), amt);
        try curve.buy(amt, 0, t) {} catch {}
        vm.stopPrank();
    }

    function sell(uint256 who, uint256 frac, bool dust) external {
        address t = traders[who % traders.length];
        uint256 bal = coin.balanceOf(t);
        if (bal == 0) return;
        uint256 amt = dust ? bound(frac, 1, bal < 1e15 ? bal : 1e15) : (bal * bound(frac, 1, 100)) / 100;
        if (amt == 0) return;
        vm.startPrank(t);
        coin.approve(address(curve), amt);
        try curve.sell(amt, 0, t) {} catch {}
        vm.stopPrank();
    }

    function donate(uint256 who, uint256 amt, bool coins) external {
        address t = traders[who % traders.length];
        vm.startPrank(t);
        if (coins) {
            uint256 bal = coin.balanceOf(t);
            if (bal > 0) coin.transfer(address(curve), bound(amt, 1, bal));
        } else {
            uint256 bal = p.balanceOf(t);
            if (bal > 0) p.transfer(address(curve), bound(amt, 1, bal));
        }
        vm.stopPrank();
    }
}

contract CurveDustInvariants is CurveHarnessBase {
    CurveDustHandler internal handler;
    address[] internal traders;
    uint256 internal minted;

    function setUp() public override {
        super.setUp();
        for (uint256 i; i < 3; ++i) {
            address t = makeAddr(string(abi.encodePacked("dust", i)));
            _fund(t);
            minted += _shares(t, 2_000e6);
            traders.push(t);
        }
        handler = new CurveDustHandler(curve, traders);
        targetContract(address(handler));
    }

    /// The curve never holds less than it tracks (donations only add), and never less
    /// than selling every circulating coin back would need.
    function invariant_solventAndBacked() public view {
        assertGe(p.balanceOf(address(curve)), curve.trackedQuote());
        assertGe(coin.balanceOf(address(curve)), curve.trackedTokens());
        uint256 outstanding = coin.SUPPLY() - curve.trackedTokens();
        uint256 gross = (outstanding * (curve.phantom() + curve.trackedQuote())) / (curve.trackedTokens() + outstanding);
        assertLe(gross, curve.trackedQuote());
        assertGe(curve.trackedTokens(), curve.reserved());
    }

    /// pToken is only ever moved, never created or destroyed, by the curve.
    function invariant_pTokenConserved() public view {
        uint256 sum = p.balanceOf(address(curve)) + p.balanceOf(platform) + p.balanceOf(address(vault));
        for (uint256 i; i < traders.length; ++i) {
            sum += p.balanceOf(traders[i]);
        }
        assertEq(sum, minted);
    }

    /// Coins are only ever moved: the whole supply is in the curve or with traders.
    function invariant_coinsConserved() public view {
        uint256 sum = coin.balanceOf(address(curve));
        for (uint256 i; i < traders.length; ++i) {
            sum += coin.balanceOf(traders[i]);
        }
        assertEq(sum, coin.SUPPLY());
    }

    /// Traders as a group: pToken held plus what selling all their coins now would fetch
    /// never exceeds what they minted.
    function invariant_tradersNeverGainAsAGroup() public view {
        uint256 pHeld;
        uint256 coins;
        for (uint256 i; i < traders.length; ++i) {
            pHeld += p.balanceOf(traders[i]);
            coins += coin.balanceOf(traders[i]);
        }
        uint256 back = coins == 0 ? 0 : curve.quoteSell(coins);
        assertLe(pHeld + back, minted);
    }
}
