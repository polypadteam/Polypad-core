// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {Router} from "../../src/Router.sol";
import {PolypadBase} from "../Polypad.t.sol";

/// @dev Looks like a BondingCurve to the Router, and keeps whatever it is handed.
contract FakeCurve {
    IERC20 public pToken;
    IERC20 public coin;
    uint256 public refundToClaim;

    constructor(IERC20 p_, IERC20 c_) {
        pToken = p_;
        coin = c_;
    }

    function setRefund(uint256 r) external {
        refundToClaim = r;
    }

    function graduated() external pure returns (bool) {
        return false;
    }

    function buy(uint256 quoteIn, uint256, address) external returns (uint256, uint256) {
        pToken.transferFrom(msg.sender, address(this), quoteIn);
        return (1, refundToClaim);
    }

    function sell(uint256 coinsIn, uint256, address) external returns (uint256) {
        coin.transferFrom(msg.sender, address(this), coinsIn);
        return refundToClaim;
    }
}

/**
 * Hostile use of the Router and the Graduator: fake curves, donations aimed at
 * graduation, and stream griefing on the FeeVault.
 */
contract AdversarialRouterGraduatorTest is PolypadBase {
    address internal eve = makeAddr("eve");
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;

    function setUp() public override {
        super.setUp();
        (coin, curve) = _launch(ID);
        p = exchange.pTokenOf(ID);
        vm.prank(owner);
        exchange.setMaxUnbacked(ID, type(uint256).max);
        usdg.mint(eve, 10_000e6);
        vm.prank(eve);
        usdg.approve(address(router), type(uint256).max);
    }

    /* ------------------------------------------------------------ fake curves */

    function test_aFakeCurveSpendsOnlyTheCallersOwnFunds() public {
        FakeCurve fake = new FakeCurve(IERC20(address(p)), IERC20(address(coin)));
        uint256 aliceUsdg = usdg.balanceOf(alice);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(eve);
        router.buy(BondingCurve(address(fake)), 100e6, 0, eve, q, sig);
        // Alice approved the Router for everything, and still has everything.
        assertEq(usdg.balanceOf(alice), aliceUsdg);
        assertEq(usdg.balanceOf(eve), 10_000e6 - 100e6, "eve paid for her own shares, which the fake kept");
        assertEq(p.balanceOf(address(router)), 0);
    }

    /// Documented: anything left in the Router (sent by mistake) can be taken by
    /// anyone with a fake curve that reports it as a refund. The Router is never
    /// meant to hold a balance between transactions.
    function test_strayPTokensInTheRouterAreAnyonesToTake() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        // Bob sends shares to the Router by mistake.
        vm.startPrank(bob);
        usdg.approve(address(exchange), 100e6);
        uint256 stray = exchange.mint(address(p), 100e6, 0, address(router), q, sig);
        vm.stopPrank();

        FakeCurve fake = new FakeCurve(IERC20(address(p)), IERC20(address(coin)));
        fake.setRefund(stray);
        // The fake (eve's) keeps what eve bought; the Router hands eve the stray shares on top.
        vm.prank(eve);
        router.buy(BondingCurve(address(fake)), 200e6, 0, eve, q, sig);
        assertEq(p.balanceOf(eve), stray);
        assertEq(p.balanceOf(address(router)), 0);
    }

    function test_aFakeCurveCannotRedeemSharesTheRouterDoesNotHold() public {
        FakeCurve fake = new FakeCurve(IERC20(address(p)), IERC20(address(coin)));
        fake.setRefund(1_000e6); // claims the sale produced 1000 shares
        _buy(eve, curve, 100e6);
        uint256 coins = coin.balanceOf(eve);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.startPrank(eve);
        coin.approve(address(router), coins);
        vm.expectRevert(); // the Router holds no shares to burn
        router.sell(BondingCurve(address(fake)), coins, 0, eve, q, sig);
        vm.stopPrank();
    }

    /* ------------------------------------------------------------ graduation */

    /// Regression: coins donated to the Graduator before a curve sells out used to
    /// count as "dust" in `coins - coinDust`, which underflowed once the donation
    /// exceeded the pool's coins, failing graduation and every retry. Dust is now
    /// measured against the balance before this graduation.
    function test_coinDonationToGraduatorCannotBlockGraduation() public {
        (uint256 got,) = _buy(bob, curve, 1_500e6);
        uint256 poolCoinsUpperBound = (curve.reserved() * 5) / 7 + 1;
        assertGt(got, poolCoinsUpperBound, "bob holds enough to grief");
        vm.prank(bob);
        coin.transfer(address(graduator), poolCoinsUpperBound);

        // Alice's buy sells the curve out.
        _buy(alice, curve, 7_000e6);
        assertTrue(curve.soldOut());
        if (!curve.graduated()) curve.graduate(); // a retry fails the same way
        assertTrue(curve.graduated(), "graduation must not be blockable by a donation");
    }

    function test_pTokenDonationToGraduatorCannotBlockGraduation() public {
        // Shares worth more than the whole raise, sent to the Graduator.
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(bob);
        usdg.approve(address(exchange), 7_000e6);
        uint256 shares = exchange.mint(address(p), 7_000e6, 0, address(graduator), q, sig);
        vm.stopPrank();
        assertGt(shares, curve.target());
        _buy(alice, curve, 7_000e6);
        if (!curve.graduated()) curve.graduate();
        assertTrue(curve.graduated(), "graduation must not be blockable by a donation");
    }

    /// A coin donation is not swept at graduation: only that graduation's own dust
    /// goes to the platform, and the donation waits for the coin's next collect().
    function test_smallCoinDonationsWaitForCollect() public {
        _buy(bob, curve, 100e6);
        vm.prank(bob);
        coin.transfer(address(graduator), 1e18);
        uint256 before = coin.balanceOf(platform);
        _buy(alice, curve, 7_000e6);
        assertTrue(curve.graduated());
        assertLt(coin.balanceOf(platform) - before, 1e18);
        assertGe(coin.balanceOf(address(graduator)), 1e18);
        graduator.collect(address(coin));
        assertEq(coin.balanceOf(address(graduator)), 0);
    }

    /* -------------------------------------------------------------- FeeVault */

    function test_withdrawUsdOfACoinBalanceRevertsAndKeepsIt() public {
        // Pool sells pay creator fees in the coin; those are not redeemable for USDG.
        (Coin c2, BondingCurve cv2) = _launch(ID, 0);
        cv2;
        uint256 amount = 1e18;
        deal(address(c2), eve, amount);
        vm.startPrank(eve);
        c2.approve(address(vault), amount);
        vault.deposit(address(c2), address(c2), amount);
        vm.stopPrank();
        uint256 owed = vault.owed(address(c2), creator);
        assertEq(owed, amount);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(PExchange.UnknownPToken.selector, address(c2)));
        vault.withdrawUsd(address(c2), creator, 0, q, sig);
        assertEq(vault.owed(address(c2), creator), owed);
    }

    /// Documented: anyone may deposit 1 unit and restart the 1h stream. Releases
    /// continue on the way (exponential, time constant 1h), so holders are delayed,
    /// not robbed; after 5h of griefing > 99% has been released.
    function test_depositSpamOnlySlowsTheStream() public {
        (Coin hc, BondingCurve hcurve) = _launch(ID, 10_000);
        _buy(alice, hcurve, 2_000e6);
        (,,, uint256 streaming0,,) = vault.books(address(hc));
        assertGt(streaming0, 0);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.startPrank(eve);
        usdg.approve(address(exchange), 10e6);
        exchange.mint(address(p), 10e6, 0, eve, q, sig);
        p.approve(address(vault), type(uint256).max);
        for (uint256 i; i < 300; ++i) {
            vm.warp(block.timestamp + 60);
            vault.deposit(address(hc), address(p), 1);
        }
        vm.stopPrank();
        (,,, uint256 streaming,,) = vault.books(address(hc));
        assertLt(streaming, streaming0 / 100 + 300);
    }
}
