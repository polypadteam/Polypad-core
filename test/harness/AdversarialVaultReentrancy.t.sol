// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {FeeVault} from "../../src/FeeVault.sol";
import {Graduator} from "../../src/Graduator.sol";
import {LaunchFactory} from "../../src/LaunchFactory.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {Router} from "../../src/Router.sol";
import {PolypadBase} from "../Polypad.t.sol";
import {CallbackUSDG} from "./AdversarialReentrancy.t.sol";

/// @dev A holder that, when its USDG rewards arrive, tries to claim again.
contract GreedyHolder {
    FeeVault public vault;
    Coin public coin;
    uint8 public mode; // 1: claim, 2: claimFor, 3: withdraw, 4: sell through the router
    bool public attempted;
    bytes public reason;
    Router public router;
    BondingCurve public curve;
    PriceOracle.Quote internal q;
    bytes internal sig;

    constructor(FeeVault v, Coin c, Router r, BondingCurve cv) {
        vault = v;
        coin = c;
        router = r;
        curve = cv;
    }

    function arm(uint8 m, PriceOracle.Quote calldata q_, bytes calldata sig_) external {
        mode = m;
        q = q_;
        sig = sig_;
        attempted = false;
    }

    function cashOut() external returns (uint256) {
        address[] memory hs = new address[](1);
        hs[0] = address(this);
        return vault.claimFor(address(coin), hs, 0, true, q, sig);
    }

    function onUsdgReceived(address, uint256) external {
        if (mode == 0 || attempted) return;
        attempted = true;
        bool ok;
        bytes memory ret;
        if (mode == 1) {
            (ok, ret) = address(vault).call(abi.encodeCall(vault.claim, (address(coin), address(this))));
        } else if (mode == 2) {
            address[] memory hs = new address[](1);
            hs[0] = address(this);
            (ok, ret) = address(vault).call(abi.encodeCall(vault.claimFor, (address(coin), hs, 0, true, q, sig)));
        } else if (mode == 3) {
            (ok, ret) = address(vault).call(abi.encodeCall(vault.withdraw, (address(coin), address(this))));
        } else if (mode == 4) {
            coin.approve(address(router), type(uint256).max);
            (ok, ret) = address(router).call(abi.encodeCall(router.sell, (curve, 1e18, 0, address(this), q, sig)));
        }
        reason = ret;
        require(!ok, "re-entry succeeded");
    }
}

/**
 * The whole stack on a collateral token with receive hooks: a holder whose
 * reward payout calls back into the vault, the exchange or the router.
 */
contract AdversarialVaultReentrancyTest is PolypadBase {
    CallbackUSDG internal cb;
    PExchange internal ex;
    LaunchFactory internal f;
    Graduator internal g;
    FeeVault internal v;
    Router internal rt;
    Coin internal c;
    BondingCurve internal cv;
    GreedyHolder internal h;

    function setUp() public override {
        super.setUp();
        cb = new CallbackUSDG();
        ex = new PExchange(owner, IERC20(address(cb)), oracle, keeper);
        f = new LaunchFactory(owner, ex, oracle, platform);
        address hookAt = address(uint160(0xaBcDEf0000000000000000000000000000000000) | 0x2000);
        deployCodeTo("Graduator.sol:Graduator", abi.encode(poolManager, address(f)), hookAt);
        g = Graduator(hookAt);
        rt = new Router(IERC20(address(cb)), ex, poolManager);
        v = new FeeVault(address(f), ex, address(poolManager));
        vm.startPrank(owner);
        ex.setRoles(keeper, address(f), bridge);
        f.setGraduator(g);
        f.setFeeVault(v);
        ex.setMaxUnbacked(ID, type(uint256).max);
        vm.stopPrank();
        cb.mint(address(ex), 100_000e6);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(creator);
        (c, cv) = f.launch(ID, "Greed", "GRD", "", 10_000, q, sig);
        h = new GreedyHolder(v, c, rt, cv);
        cb.mint(alice, 10_000e6);
        vm.startPrank(alice);
        cb.approve(address(rt), type(uint256).max);
        rt.buy(cv, 1_000e6, 0, address(h), q, sig);
        rt.buy(cv, 1_000e6, 0, alice, q, sig);
        vm.stopPrank();
        vm.warp(block.timestamp + 2 hours);
        // Someone else trades, so the stream has released rewards to claim.
        (q, sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        rt.buy(cv, 100e6, 0, alice, q, sig);
        vm.warp(block.timestamp + 2 hours);
    }

    function _check(uint8 mode) internal {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        h.arm(mode, q, sig);
        uint256 pending = v.pending(address(c), address(h));
        assertGt(pending, 0);
        uint256 paid = h.cashOut();
        assertEq(paid, pending);
        assertTrue(h.attempted(), "payout reached the holder");
        assertEq(bytes4(h.reason()), ReentrancyGuard.ReentrancyGuardReentrantCall.selector);
        assertEq(v.pending(address(c), address(h)), 0);
    }

    function test_reclaimingFromInsideAUsdgRewardIsBlocked() public {
        _check(1);
    }

    function test_reclaimForFromInsideAUsdgRewardIsBlocked() public {
        _check(2);
    }

    function test_creatorWithdrawFromInsideAUsdgRewardIsBlocked() public {
        _check(3);
    }

    /// The exchange is locked for the whole redemption, so the router's sell
    /// (which redeems) cannot run inside the payout either.
    function test_sellingThroughTheRouterFromInsideARewardIsBlocked() public {
        _check(4);
    }

    /// A third party asking for USDG on someone's behalf pays them in shares:
    /// no sale, no USDG, so no callback into the holder at all.
    function test_aThirdPartyCashOutPaysSharesWithoutCallback() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        h.arm(4, q, sig);
        address[] memory hs = new address[](1);
        hs[0] = address(h);
        uint256 pending = v.pending(address(c), address(h));
        vm.prank(bob);
        uint256 paid = v.claimFor(address(c), hs, 0, true, q, sig);
        assertEq(paid, pending);
        assertEq(ex.pTokenOf(ID).balanceOf(address(h)), pending);
        assertFalse(h.attempted());
    }
}
