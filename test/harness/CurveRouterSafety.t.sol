// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {PoolKey} from "v4-core/src/types/PoolKey.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Graduator} from "../../src/Graduator.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {Router} from "../../src/Router.sol";

import {CurveHarnessBase} from "./CurveHarnessBase.sol";

contract Junk is ERC20 {
    constructor() ERC20("Junk", "J") {
        _mint(msg.sender, 1e30);
    }
}

/// @dev A curve that lies: reports whatever tokens it likes, pulls whatever the Router
///      approved, and can try to re-enter the Router or drain a victim who approved it.
contract FakeCurve {
    IERC20 public pToken;
    IERC20 public coin;
    Router internal router;
    uint256 internal fakeOut;
    bool internal reenter;
    address internal thief;

    constructor(IERC20 pToken_, IERC20 coin_, Router router_, address thief_) {
        pToken = pToken_;
        coin = coin_;
        router = router_;
        thief = thief_;
    }

    function setFakeOut(uint256 v) external {
        fakeOut = v;
    }

    function setReenter(bool v) external {
        reenter = v;
    }

    function graduated() external pure returns (bool) {
        return false;
    }

    function graduator() external view returns (address) {
        return address(this);
    }

    function buy(uint256 quoteIn, uint256, address) external returns (uint256, uint256) {
        pToken.transferFrom(msg.sender, thief, quoteIn);
        if (reenter) {
            PriceOracle.Quote memory q;
            router.buy(BondingCurve(address(this)), 1, 0, thief, q, "");
        }
        return (1, 0);
    }

    function sell(uint256, uint256, address) external view returns (uint256) {
        return fakeOut;
    }
}

/// @dev What a malicious or fake `curve` argument can and cannot do through the Router,
///      the pool callback gates, and that the Router never keeps anything.
contract CurveRouterSafetyTest is CurveHarnessBase {
    Junk internal junk;

    function setUp() public override {
        super.setUp();
        junk = new Junk();
        junk.transfer(attacker, 1e24);
    }

    /// A fake curve can only ever spend the caller's own funds: an attacker cannot use
    /// the Router to pull from someone else's (unlimited) Router approval.
    function test_fakeCurveCannotTouchOtherUsersApprovals() public {
        // alice and victim approved the Router for everything in setUp.
        uint256 a0 = usdg.balanceOf(alice);
        uint256 v0 = usdg.balanceOf(victim);
        FakeCurve fake = new FakeCurve(IERC20(address(p)), IERC20(address(usdg)), router, attacker);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(attacker);
        router.buy(BondingCurve(address(fake)), 100e6, 0, attacker, q, sig);

        // Sell with coin() = USDG: pulls USDG from the caller (attacker), nobody else.
        vm.startPrank(attacker);
        usdg.approve(address(router), 50e6);
        fake.setFakeOut(0);
        vm.expectRevert();
        router.sell(BondingCurve(address(fake)), 50e6, 0, attacker, q, sig);
        vm.stopPrank();

        assertEq(usdg.balanceOf(alice), a0);
        assertEq(usdg.balanceOf(victim), v0);
        _assertRouterEmpty();
    }

    /// A fake curve re-entering the Router mid-trade is refused.
    function test_fakeCurveCannotReenterTheRouter() public {
        FakeCurve fake = new FakeCurve(IERC20(address(p)), IERC20(address(junk)), router, attacker);
        fake.setReenter(true);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(attacker);
        vm.expectRevert(); // ReentrancyGuardReentrantCall
        router.buy(BondingCurve(address(fake)), 100e6, 0, attacker, q, sig);
    }

    /// The Router is stateless: pToken left in it by mistake can be taken by anyone
    /// through a fake curve (redeemed as the Router, paid to the caller). Known and
    /// accepted for a router that holds nothing between transactions; recorded here so
    /// integrators never send tokens to it.
    function test_INFO_strayPTokenInTheRouterIsTakeableByAnyone() public {
        uint256 stray = _shares(bob, 100e6);
        vm.prank(bob);
        p.transfer(address(router), stray);

        FakeCurve fake = new FakeCurve(IERC20(address(p)), IERC20(address(junk)), router, attacker);
        fake.setFakeOut(stray);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 u0 = usdg.balanceOf(attacker);
        vm.startPrank(attacker);
        junk.approve(address(router), 1);
        router.sell(BondingCurve(address(fake)), 1, 0, attacker, q, sig);
        vm.stopPrank();
        assertGt(usdg.balanceOf(attacker), u0);
        assertEq(p.balanceOf(address(router)), 0);
    }

    function test_callbacksOnlyFromThePoolManager() public {
        vm.expectRevert(Router.OnlyPoolManager.selector);
        router.unlockCallback("");
        vm.expectRevert(Graduator.OnlyPoolManager.selector);
        graduator.unlockCallback("");
        PoolKey memory key;
        vm.expectRevert(Graduator.OnlyPoolManager.selector);
        graduator.beforeInitialize(address(this), key, 0);
    }

    function test_quotePoolBeforeGraduationReverts() public {
        vm.expectRevert(abi.encodeWithSelector(Graduator.NotGraduated.selector, address(coin)));
        router.quotePool(curve, true, 1e6);
    }

    /// quotePool never changes state (it simulates by swapping and reverting).
    function testFuzz_quotePoolChangesNothing(uint256 amt, bool buySide) public {
        _graduateDirect();
        amt = bound(amt, 1, 1e30);
        uint256 spot = curve.spotPrice();
        try router.quotePool(curve, buySide, amt) {} catch {}
        assertEq(curve.spotPrice(), spot);
        _assertRouterEmpty();
    }

    /// Random Router trades across the whole lifecycle leave the Router with nothing.
    function testFuzz_routerNeverKeepsTokens(uint256[6] memory sizes) public {
        for (uint256 i; i < sizes.length; ++i) {
            uint256 s = bound(sizes[i], 1e6, 6_000e6);
            if (i % 2 == 0) {
                _routerBuy(alice, s, 0);
            } else {
                uint256 coins = coin.balanceOf(alice);
                uint256 amt = (coins * bound(sizes[i], 1, 100)) / 100;
                if (amt == 0) continue;
                (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
                vm.startPrank(alice);
                coin.approve(address(router), amt);
                try router.sell(curve, amt, 0, alice, q, sig) {} catch {}
                vm.stopPrank();
            }
            _assertRouterEmpty();
        }
    }

    /// If graduation fails mid-buy, the Router refunds the unused pToken to the recipient
    /// and keeps nothing.
    function test_routerRefundsWhenGraduationFails() public {
        vm.mockCallRevert(address(graduator), abi.encodeWithSelector(Graduator.graduate.selector), "boom");
        (uint256 coins, uint256 refund) = _routerBuy(alice, 9_000e6, 0);
        assertGt(coins, 0);
        assertGt(refund, 0);
        assertEq(p.balanceOf(alice), refund);
        _assertRouterEmpty();
    }
}
