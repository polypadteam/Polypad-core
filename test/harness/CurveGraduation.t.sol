// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {Graduator} from "../../src/Graduator.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";

import {CurveHarnessBase} from "./CurveHarnessBase.sol";

/// @dev Graduation: the exact sell-out, refunds, conservation of every token, the gas
///      floor, failure and retry, and griefing the Graduator.
contract CurveGraduationTest is CurveHarnessBase {
    uint256 internal constant BPS = 10_000;
    address internal constant DEAD = 0x000000000000000000000000000000000000dEaD;

    /// pToken (gross, fee included) that buys exactly the sellable remainder.
    function _exactSellOut() internal view returns (uint256 gross) {
        uint256 t = curve.trackedTokens();
        uint256 out = t - curve.reserved();
        uint256 v = curve.phantom() + curve.trackedQuote();
        uint256 net = (v * out + (t - out) - 1) / (t - out);
        uint256 fee = curve.feeBps();
        gross = (net * BPS + (BPS - fee) - 1) / (BPS - fee);
    }

    /// The smallest payment that sells the curve out graduates it with no refund beyond
    /// rounding; one unit less leaves the curve open. (Fee rounding can make that payment
    /// a unit or two under the fee-inclusive price `_exactSellOut`; the curve then takes
    /// the shortfall out of the fee, never out of the price.)
    function testFuzz_exactSellOutBoundary(uint256 pre) public {
        pre = bound(pre, 0, 5_000e6);
        if (pre >= 2e6) _curveBuy(alice, _shares(alice, (pre * 6) / 10), 0);
        uint256 sellable = curve.trackedTokens() - curve.reserved();
        uint256 need = _exactSellOut();
        uint256 min = need;
        while (curve.quoteBuy(min - 1) >= sellable) --min;
        assertLe(need - min, 3, "sell-out price far from the fee-inclusive price");
        _shares(bob, 8_000e6);

        uint256 snap = vm.snapshotState();
        _curveBuy(bob, min - 1, 0);
        assertFalse(curve.graduated(), "under the sell-out graduated");
        assertGt(curve.trackedTokens(), curve.reserved());
        vm.revertToState(snap);

        uint256 b0 = p.balanceOf(bob);
        uint256 vault0 = p.balanceOf(address(vault));
        uint256 plat0 = p.balanceOf(platform);
        (uint256 out, uint256 refund) = _curveBuy(bob, min, 0);
        assertTrue(curve.graduated());
        assertEq(out, sellable);
        assertEq(refund, 0);
        assertEq(b0 - p.balanceOf(bob), min);
        // The fee, possibly trimmed by up to the rounding gap, still went out.
        assertGt(p.balanceOf(address(vault)) - vault0, 0);
        assertGt(p.balanceOf(platform) - plat0, 0);
    }

    /// Overpaying: the buyer is charged exactly the sell-out price and refunded the rest,
    /// and every pToken and coin is accounted for: pool + platform dust + fees on one side,
    /// what the curve held + what the buyer paid on the other.
    function testFuzz_sellOutConservesEveryToken(uint256 pre, uint256 over, uint16 holdersBps) public {
        holdersBps = uint16(bound(holdersBps, 0, 10_000));
        if (holdersBps > 0) {
            (coin, curve) = _launch(ID, holdersBps);
        }
        pre = bound(pre, 0, 5_000e6);
        if (pre >= 2e6) _curveBuy(alice, _shares(alice, (pre * 6) / 10), 0);
        uint256 need = _exactSellOut();
        over = bound(over, 0, 20_000e6);
        _shares(bob, ((need + over) * 12) / 10 / 1 + 1e6);
        uint256 pIn = need + over;
        if (pIn > p.balanceOf(bob)) pIn = p.balanceOf(bob);

        uint256 q0 = curve.trackedQuote();
        uint256 t0 = curve.trackedTokens();
        uint256 pm0 = p.balanceOf(address(poolManager));
        uint256 plat0 = p.balanceOf(platform);
        uint256 vault0 = p.balanceOf(address(vault));
        uint256 cPm0 = coin.balanceOf(address(poolManager));
        uint256 b0 = p.balanceOf(bob);

        (uint256 out, uint256 refund) = _curveBuy(bob, pIn, 0);
        assertTrue(curve.graduated());
        uint256 spent = b0 - p.balanceOf(bob);
        assertEq(spent, pIn - refund);
        assertLe(spent, need, "charged more than the sell-out price");

        uint256 pOut = (p.balanceOf(address(poolManager)) - pm0) + (p.balanceOf(platform) - plat0)
            + (p.balanceOf(address(vault)) - vault0);
        assertEq(pOut, q0 + spent, "pToken created or lost at graduation");

        uint256 cOut = (coin.balanceOf(address(poolManager)) - cPm0) + coin.balanceOf(DEAD) + coin.balanceOf(platform)
            + out;
        assertEq(cOut, t0, "coins created or lost at graduation");
        assertEq(p.balanceOf(address(curve)), 0);
        assertEq(coin.balanceOf(address(curve)), 0);
        assertEq(p.balanceOf(address(graduator)), 0);
        assertEq(coin.balanceOf(address(graduator)), 0);
    }

    function extBuy(BondingCurve c, uint256 pIn) external returns (uint256 out) {
        (out,) = c.buy(pIn, 0, address(this));
    }

    /// Sweep the gas given to a graduating buy: it either reverts, or it graduates. It never
    /// succeeds with the curve left sold out and no pool (the griefing the floor prevents).
    function testFuzz_gasFloorNeverLeavesASoldOutCurveWithoutAPool(uint256 gas, uint16 holdersBps) public {
        holdersBps = uint16(bound(holdersBps, 0, 10_000));
        if (holdersBps > 0) (coin, curve) = _launch(ID, holdersBps);
        gas = bound(gas, 150_000, 3_000_000);
        _fund(address(this));
        uint256 pIn = _shares(address(this), 8_000e6);
        p.approve(address(curve), pIn);
        try this.extBuyWithGas(curve, pIn, gas) {
            assertTrue(curve.graduated(), "graduating buy succeeded without a pool");
        } catch {
            assertFalse(curve.graduated());
            assertFalse(curve.soldOut(), "a reverted buy left the curve sold out");
        }
    }

    function extBuyWithGas(BondingCurve c, uint256 pIn, uint256 gas) external {
        c.buy{gas: gas}(pIn, 0, address(this));
    }

    /// The same sweep through the Router, where the refund then buys in the pool.
    function testFuzz_gasFloorThroughTheRouter(uint256 gas, uint16 holdersBps) public {
        holdersBps = uint16(bound(holdersBps, 0, 10_000));
        if (holdersBps > 0) (coin, curve) = _launch(ID, holdersBps);
        gas = bound(gas, 300_000, 4_000_000);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        try router.buy{gas: gas}(curve, 9_000e6, 0, alice, q, sig) {
            assertTrue(curve.graduated(), "router buy sold out without a pool");
        } catch {
            assertFalse(curve.soldOut());
        }
        _assertRouterEmpty();
    }

    /// A buy that does not reach the sell-out is not subject to the floor.
    function test_ordinaryBuyNeedsNoGraduationGas() public {
        _fund(address(this));
        uint256 pIn = _shares(address(this), 100e6);
        p.approve(address(curve), pIn);
        this.extBuyWithGas(curve, pIn, 300_000);
        assertGt(coin.balanceOf(address(this)), 0);
    }

    /// Graduation that fails for any reason: the buy stands, the curve is sold out, the
    /// approvals are cleared, buying is closed, selling works, and anyone can retry.
    function test_failedGraduationIsRetryableByAnyone() public {
        vm.mockCallRevert(address(graduator), abi.encodeWithSelector(Graduator.graduate.selector), "boom");
        (uint256 out,) = _curveBuy(bob, _shares(bob, 8_000e6), 0);
        assertGt(out, 0);
        assertFalse(curve.graduated());
        assertTrue(curve.soldOut());
        assertEq(p.allowance(address(curve), address(graduator)), 0);
        assertEq(coin.allowance(address(curve), address(graduator)), 0);

        uint256 more = _shares(alice, 10e6);
        vm.startPrank(alice);
        p.approve(address(curve), more);
        vm.expectRevert(BondingCurve.SoldOut_.selector);
        curve.buy(more, 0, alice);
        vm.stopPrank();

        // Still broken: retry fails the same way, state unchanged.
        vm.prank(attacker);
        curve.graduate();
        assertFalse(curve.graduated());

        vm.clearMockedCalls();
        vm.prank(attacker);
        curve.graduate();
        assertTrue(curve.graduated());
        vm.expectRevert(BondingCurve.Graduated_.selector);
        curve.graduate();
    }

    /// After a failed graduation a seller reopens the curve; the next sell-out graduates.
    function test_sellAfterFailedGraduationThenRegraduate() public {
        vm.mockCallRevert(address(graduator), abi.encodeWithSelector(Graduator.graduate.selector), "boom");
        (uint256 out,) = _curveBuy(bob, _shares(bob, 8_000e6), 0);
        _curveSell(bob, out / 10, 0);
        assertFalse(curve.soldOut());
        vm.clearMockedCalls();
        _curveBuy(bob, p.balanceOf(bob), 0);
        assertTrue(curve.graduated());
    }

    function test_graduateBeforeSellOutReverts() public {
        vm.expectRevert(BondingCurve.NotSoldOut.selector);
        curve.graduate();
    }

    function test_noCurveTradingAfterGraduation() public {
        _graduateDirect();
        uint256 more = _shares(alice, 10e6);
        vm.startPrank(alice);
        p.approve(address(curve), more);
        vm.expectRevert(BondingCurve.Graduated_.selector);
        curve.buy(more, 0, alice);
        vm.expectRevert(BondingCurve.Graduated_.selector);
        curve.sell(1e18, 0, alice);
        vm.stopPrank();
        assertEq(curve.quoteBuy(1e6), 0);
        assertEq(curve.quoteSell(1e18), 0);
    }

    /// Anyone can send pToken to the Graduator. Graduation must not depend on the
    /// Graduator's balance before the curve's own tokens arrive.
    function test_pTokenDonationToGraduatorCannotBlockGraduation() public {
        // The pToken for this market is shared by every coin on it, so a donation here
        // is aimed at every coin on the market.
        uint256 donation = _shares(attacker, 8_000e6); // > the curve's raise (~10k shares)
        assertGt(donation, curve.target());
        vm.prank(attacker);
        p.transfer(address(graduator), donation);

        _curveBuy(bob, _shares(bob, 8_000e6), 0);
        // Regression: `pTokens - pDust` used to underflow inside Graduator.graduate
        // (dust taken from the whole balance), leaving the curve sold out for good.
        assertTrue(curve.graduated(), "graduation blocked by a donation to the Graduator");
    }

    function test_coinDonationToGraduatorCannotBlockGraduation() public {
        (uint256 coins,) = _curveBuy(attacker, _shares(attacker, 5_500e6), 0);
        // The pool will take reserved x q / (phantom + q) coins; donate more than that.
        uint256 poolCoins = (curve.reserved() * curve.target()) / (curve.phantom() + curve.target());
        vm.assume(coins > poolCoins);
        vm.prank(attacker);
        coin.transfer(address(graduator), poolCoins + 1);
        _curveBuy(bob, _shares(bob, 8_000e6), 0);
        assertTrue(curve.graduated(), "graduation blocked by a coin donation to the Graduator");
    }

    /// A donation is not this graduation's dust: it is neither swept to the platform
    /// at graduation nor in the way of the math. It stays in the Graduator until a
    /// collect() on a graduated coin sharing that token pays it out as pool fees.
    function test_donationToGraduatorStaysUntilCollectPaysItAsFees() public {
        uint256 donation = _shares(attacker, 100e6);
        uint256 plat0 = p.balanceOf(platform);
        uint256 vault0 = p.balanceOf(address(vault));
        // What the platform gets from this graduation (curve fees + dust) without a donation...
        uint256 snap = vm.snapshotState();
        _graduateDirect();
        uint256 platNoDonation = p.balanceOf(platform) - plat0;
        vm.revertToState(snap);
        // ...is exactly what it gets with one: the donation is not swept.
        vm.prank(attacker);
        p.transfer(address(graduator), donation);
        _graduateDirect();
        assertTrue(curve.graduated());
        assertEq(p.balanceOf(platform) - plat0, platNoDonation);
        assertEq(p.balanceOf(address(graduator)), donation);

        uint256 plat1 = p.balanceOf(platform);
        graduator.collect(address(coin));
        assertEq(p.balanceOf(address(graduator)), 0);
        uint256 paid = (p.balanceOf(platform) - plat1) + (p.balanceOf(address(vault)) - vault0);
        assertGe(paid, donation, "the donation is paid out as fees");
    }

    /// Only a factory curve may graduate, and only once per coin.
    function test_onlyCurvesGraduateOnce() public {
        vm.expectRevert(Graduator.OnlyCurve.selector);
        graduator.graduate(
            IERC20(address(coin)), IERC20(address(p)), 1, 1, address(vault), platform, 10_000, 5_000
        );
        _graduateDirect();
        vm.prank(address(curve));
        vm.expectRevert(abi.encodeWithSelector(Graduator.AlreadyGraduated.selector, address(coin)));
        graduator.graduate(
            IERC20(address(coin)), IERC20(address(p)), 1, 1, address(vault), platform, 10_000, 5_000
        );
    }
}
