// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IHooks} from "v4-core/src/interfaces/IHooks.sol";
import {Currency} from "v4-core/src/types/Currency.sol";
import {PoolKey} from "v4-core/src/types/PoolKey.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {FeeVault} from "../../src/FeeVault.sol";
import {Fees} from "../../src/Fees.sol";
import {Graduator} from "../../src/Graduator.sol";
import {LaunchFactory} from "../../src/LaunchFactory.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {Router} from "../../src/Router.sol";
import {PolypadBase} from "../Polypad.t.sol";

contract Stray is ERC20 {
    constructor() ERC20("Stray", "STRAY") {
        _mint(msg.sender, 1e24);
    }
}

/**
 * Access matrix: every external state-changing function of every contract,
 * called by an outsider (must revert) and by the role it is meant for (must work).
 */
contract AccessMatrixTest is PolypadBase {
    address internal eve = makeAddr("eve");
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;

    function setUp() public override {
        super.setUp();
        (coin, curve) = _launch(ID, 5_000);
        p = exchange.pTokenOf(ID);
    }

    function _ids(uint256 id) internal pure returns (uint256[] memory a) {
        a = new uint256[](1);
        a[0] = id;
    }

    function _one(uint256 x) internal pure returns (uint256[] memory a) {
        a = new uint256[](1);
        a[0] = x;
    }

    function _u64(uint64 x) internal pure returns (uint64[] memory a) {
        a = new uint64[](1);
        a[0] = x;
    }

    function _notOwner(address who) internal {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, who));
    }

    /* ------------------------------------------------------------ PriceOracle */

    function test_oracle_ownerOnly() public {
        vm.startPrank(eve);
        _notOwner(eve);
        oracle.setSigner(eve);
        _notOwner(eve);
        oracle.acceptSigner();
        _notOwner(eve);
        oracle.setKeeper(eve);
        _notOwner(eve);
        oracle.transferOwnership(eve);
        // Renouncing is disabled for everyone, owner included.
        vm.expectRevert(PriceOracle.BadParams.selector);
        oracle.renounceOwnership();
        vm.stopPrank();

        vm.startPrank(owner);
        oracle.setKeeper(keeper);
        oracle.setSigner(eve);
        vm.warp(block.timestamp + oracle.ROLE_DELAY());
        oracle.acceptSigner();
        vm.stopPrank();
        assertEq(oracle.signer(), eve);
    }

    function test_oracle_keeperOnly() public {
        vm.startPrank(eve);
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.setPaused(_ids(ID), true);
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.stopPrank();
        // The owner is not the keeper either.
        vm.prank(owner);
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());

        vm.startPrank(keeper);
        oracle.setPaused(_ids(ID), true);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.stopPrank();
    }

    function test_oracle_ownershipIsTwoStep() public {
        vm.prank(owner);
        oracle.transferOwnership(eve);
        assertEq(oracle.owner(), owner, "not until accepted");
        vm.prank(bob);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, bob));
        oracle.acceptOwnership();
        vm.prank(eve);
        oracle.acceptOwnership();
        assertEq(oracle.owner(), eve);
    }

    /* ------------------------------------------------------------- PExchange */

    function test_exchange_ownerOnly() public {
        Stray stray = new Stray();
        stray.transfer(address(exchange), 1e18);
        vm.startPrank(eve);
        _notOwner(eve);
        exchange.setRoles(eve, eve, bridge);
        _notOwner(eve);
        exchange.proposeBridgeDeposit(eve);
        _notOwner(eve);
        exchange.acceptBridgeDeposit();
        _notOwner(eve);
        exchange.setParams(25, 25, 50_000, 950_000, 1);
        _notOwner(eve);
        exchange.setMaxRisk(ID, 1);
        _notOwner(eve);
        exchange.setMaxPrice(ID, 990_000);
        _notOwner(eve);
        exchange.setSettleFee(0);
        _notOwner(eve);
        exchange.setOutflowCap(0, 0);
        _notOwner(eve);
        exchange.cancel(0);
        _notOwner(eve);
        exchange.resume();
        _notOwner(eve);
        exchange.rescue(IERC20(address(stray)), eve, 1e18);
        _notOwner(eve);
        exchange.transferOwnership(eve);
        vm.expectRevert(PExchange.BadParams.selector);
        exchange.renounceOwnership();
        vm.stopPrank();

        vm.startPrank(owner);
        exchange.setParams(25, 25, 50_000, 950_000, 2_000e6);
        exchange.setMaxRisk(ID, 1);
        exchange.setMaxPrice(ID, 990_000);
        exchange.setSettleFee(50);
        exchange.setOutflowCap(1_000_000e6, 5_000);
        exchange.rescue(IERC20(address(stray)), owner, 1e18);
        exchange.halt();
        exchange.resume();
        vm.stopPrank();
        assertEq(stray.balanceOf(owner), 1e18);
    }

    function test_exchange_keeperOnly() public {
        vm.startPrank(eve);
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.reportBacked(_ids(ID), _one(1));
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.sendToBridge(1);
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.halt();
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.setShareLabel(ID, "Yes", "pYES");
        vm.stopPrank();
        // The owner may not report backing or move the float.
        vm.startPrank(owner);
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.reportBacked(_ids(ID), _one(1));
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.sendToBridge(1);
        vm.stopPrank();

        vm.startPrank(keeper);
        exchange.reportBacked(_ids(ID), _one(1));
        exchange.sendToBridge(1);
        exchange.setShareLabel(ID, "Yes", "pYES");
        exchange.halt();
        // The keeper can halt but never resume.
        _notOwner(keeper);
        exchange.resume();
        vm.stopPrank();
        assertEq(usdg.balanceOf(bridge), 1);
    }

    function test_exchange_factoryOrOwnerCreatesPTokens() public {
        vm.prank(eve);
        vm.expectRevert(PExchange.OnlyFactory.selector);
        exchange.ensurePToken(123);
        vm.prank(keeper);
        vm.expectRevert(PExchange.OnlyFactory.selector);
        exchange.ensurePToken(123);
        vm.prank(address(factory));
        PToken a = exchange.ensurePToken(123);
        vm.prank(owner);
        assertEq(address(exchange.ensurePToken(123)), address(a), "idempotent");
        assertEq(exchange.pTokenAddress(123), address(a));
    }

    function test_exchange_publicFunctionsNeedNoRole() public {
        // absorb and payQueue are open to anyone and harmless with nothing to do.
        vm.startPrank(eve);
        assertEq(exchange.absorb(address(p)), 0);
        assertEq(exchange.payQueue(10), 0);
        vm.expectRevert(PExchange.ZeroAmount.selector);
        exchange.withdrawUnclaimed(eve);
        // No delayed sale yet: nothing to release.
        vm.expectRevert();
        exchange.release(0);
        vm.stopPrank();
    }

    /// A sale over the hourly cap waits: anyone releases it after DELAY (not while
    /// halted), and only the owner can cancel one before that.
    function test_exchange_releaseAnyoneCancelOwnerOnly() public {
        vm.prank(owner);
        exchange.setOutflowCap(0, 0); // every sale is over the cap
        (PriceOracle.Quote memory bq, bytes memory bsig) = signedQuote(ID, BUY);
        vm.startPrank(alice);
        usdg.approve(address(exchange), type(uint256).max);
        uint256 got = exchange.mint(address(p), 100e6, 0, alice, bq, bsig);
        (PriceOracle.Quote memory sq, bytes memory ssig) = signedQuote(ID, SELL);
        exchange.redeem(address(p), got / 2, 0, alice, sq, ssig);
        exchange.redeem(address(p), got / 2, 0, alice, sq, ssig);
        vm.stopPrank();
        assertEq(exchange.delayedShares(ID), (got / 2) * 2);

        vm.startPrank(eve);
        vm.expectRevert(abi.encodeWithSelector(PExchange.NotYet.selector, block.timestamp + exchange.DELAY()));
        exchange.release(0);
        _notOwner(eve);
        exchange.cancel(0);
        vm.stopPrank();
        vm.prank(keeper);
        _notOwner(keeper);
        exchange.cancel(0);

        vm.warp(block.timestamp + exchange.DELAY());
        vm.prank(keeper);
        exchange.halt();
        vm.prank(eve);
        vm.expectRevert(PExchange.Halted.selector);
        exchange.release(0);
        vm.prank(owner);
        exchange.resume();

        uint256 before = usdg.balanceOf(alice);
        vm.prank(eve);
        exchange.release(0);
        assertGt(usdg.balanceOf(alice), before, "released to the seller, whoever calls");
        vm.prank(eve);
        vm.expectRevert(PExchange.NothingPending.selector);
        exchange.release(0);

        vm.prank(owner);
        exchange.cancel(1);
        assertEq(p.balanceOf(alice), got - (got / 2) * 2 + got / 2, "cancel gives the pToken back");
        assertEq(exchange.delayedShares(ID), 0);
        vm.prank(owner);
        vm.expectRevert(PExchange.NothingPending.selector);
        exchange.cancel(1);
    }

    /* ---------------------------------------------------------------- PToken */

    function test_ptoken_exchangeOnly() public {
        vm.startPrank(eve);
        vm.expectRevert(PToken.OnlyExchange.selector);
        p.mint(eve, 1);
        vm.expectRevert(PToken.OnlyExchange.selector);
        p.burn(alice, 1);
        vm.expectRevert(PToken.OnlyExchange.selector);
        p.setLabel("x", "y");
        vm.stopPrank();
        // Not even the owner or keeper directly.
        vm.prank(owner);
        vm.expectRevert(PToken.OnlyExchange.selector);
        p.mint(owner, 1);
        vm.prank(keeper);
        vm.expectRevert(PToken.OnlyExchange.selector);
        p.setLabel("x", "y");
        assertEq(p.exchange(), address(exchange));
    }

    /* ---------------------------------------------------------- LaunchFactory */

    function test_factory_ownerOnly() public {
        Fees memory f = Fees(140, 5_000, 10_000, 5_000);
        vm.startPrank(eve);
        _notOwner(eve);
        factory.setConfig(eve, 1);
        _notOwner(eve);
        factory.setGraduator(Graduator(eve));
        _notOwner(eve);
        factory.setFees(f);
        _notOwner(eve);
        factory.setFeeVault(FeeVault(eve));
        _notOwner(eve);
        factory.transferOwnership(eve);
        vm.stopPrank();

        vm.startPrank(owner);
        factory.setConfig(platform, 6_000e6);
        factory.setFees(f);
        factory.setGraduator(graduator);
        factory.setFeeVault(vault);
        vm.stopPrank();
    }

    /* ----------------------------------------------------------- BondingCurve */

    function test_curve_initializeOnceByFactoryOnly() public {
        vm.prank(eve);
        vm.expectRevert(BondingCurve.OnlyFactory.selector);
        curve.initialize(IERC20(address(usdg)));
        vm.prank(address(factory));
        vm.expectRevert(BondingCurve.AlreadyInitialized.selector);
        curve.initialize(IERC20(address(usdg)));
    }

    function test_curve_graduateRetryNeedsASoldOutCurve() public {
        vm.prank(eve);
        vm.expectRevert(BondingCurve.NotSoldOut.selector);
        curve.graduate();
    }

    /* -------------------------------------------------------------- Graduator */

    function test_graduator_onlyCurvesGraduate() public {
        vm.prank(eve);
        vm.expectRevert(Graduator.OnlyCurve.selector);
        graduator.graduate(IERC20(address(coin)), IERC20(address(p)), 1, 1, address(vault), eve, 10_000, 5_000);
        // The factory itself is not a curve.
        vm.prank(address(factory));
        vm.expectRevert(Graduator.OnlyCurve.selector);
        graduator.graduate(IERC20(address(coin)), IERC20(address(p)), 1, 1, address(vault), eve, 10_000, 5_000);
    }

    function test_graduator_callbacksOnlyFromThePoolManager() public {
        PoolKey memory key = PoolKey({
            currency0: Currency.wrap(address(coin)),
            currency1: Currency.wrap(address(p)),
            fee: 10_000,
            tickSpacing: 200,
            hooks: IHooks(address(graduator))
        });
        vm.prank(eve);
        vm.expectRevert(Graduator.OnlyPoolManager.selector);
        graduator.unlockCallback(abi.encode(key, uint128(1)));
        vm.prank(eve);
        vm.expectRevert(Graduator.OnlyPoolManager.selector);
        graduator.beforeInitialize(address(graduator), key, 1);
        // From the PoolManager, but a pool someone else is creating with our hook.
        vm.prank(address(poolManager));
        vm.expectRevert(Graduator.OnlySelf.selector);
        graduator.beforeInitialize(eve, key, 1);
        vm.prank(address(poolManager));
        assertEq(graduator.beforeInitialize(address(graduator), key, 1), IHooks.beforeInitialize.selector);
    }

    function test_graduator_nobodyCanSquatAPoolWithOurHook() public {
        (address c0, address c1) = address(coin) < address(p) ? (address(coin), address(p)) : (address(p), address(coin));
        PoolKey memory key = PoolKey({
            currency0: Currency.wrap(c0),
            currency1: Currency.wrap(c1),
            fee: 10_000,
            tickSpacing: 200,
            hooks: IHooks(address(graduator))
        });
        vm.prank(eve);
        vm.expectRevert();
        poolManager.initialize(key, 79228162514264337593543950336);
    }

    function test_graduator_collectNeedsAGraduatedCoin() public {
        vm.prank(eve);
        vm.expectRevert(abi.encodeWithSelector(Graduator.NotGraduated.selector, address(coin)));
        graduator.collect(address(coin));
    }

    /* --------------------------------------------------------------- FeeVault */

    function test_vault_registerFactoryOnly() public {
        vm.prank(eve);
        vm.expectRevert(FeeVault.OnlyFactory.selector);
        vault.register(eve, eve, eve, eve, eve, 0);
        // Re-registering an existing coin, even by the factory, is refused.
        vm.prank(address(factory));
        vm.expectRevert(FeeVault.BadParams.selector);
        vault.register(address(coin), eve, eve, eve, eve, 0);
    }

    function test_vault_setPayeePayeeOnly() public {
        vm.prank(eve);
        vm.expectRevert(FeeVault.OnlyPayee.selector);
        vault.setPayee(address(coin), eve);
        vm.prank(owner);
        vm.expectRevert(FeeVault.OnlyPayee.selector);
        vault.setPayee(address(coin), owner);
        vm.prank(creator);
        vm.expectRevert(FeeVault.BadParams.selector);
        vault.setPayee(address(coin), address(0));
        vm.prank(creator);
        vault.setPayee(address(coin), bob);
        // The old payee has lost the right to move it again.
        vm.prank(creator);
        vm.expectRevert(FeeVault.OnlyPayee.selector);
        vault.setPayee(address(coin), creator);
        // An unknown coin has no payee; address(0) cannot call.
        vm.prank(eve);
        vm.expectRevert(FeeVault.OnlyPayee.selector);
        vault.setPayee(eve, eve);
    }

    function test_vault_onTransferFromAStrangerDoesNothing() public {
        _buy(alice, curve, 1_000e6);
        (uint256 acc, uint256 supply,,,,) = vault.books(address(coin));
        vm.prank(eve);
        vault.onTransfer(alice, eve);
        (uint256 acc2, uint256 supply2,,,,) = vault.books(address(coin));
        assertEq(acc, acc2);
        assertEq(supply, supply2);
        assertEq(vault.tracked(address(coin), eve), 0);
    }

    function test_vault_depositForUnknownCoinOrAssetRefused() public {
        vm.prank(eve);
        vm.expectRevert(abi.encodeWithSelector(FeeVault.Unknown.selector, eve));
        vault.deposit(eve, address(p), 1);
        vm.prank(eve);
        vm.expectRevert(abi.encodeWithSelector(FeeVault.BadAsset.selector, address(usdg)));
        vault.deposit(address(coin), address(usdg), 1);
    }

    function test_vault_withdrawIsOwnFundsOnly() public {
        _buy(alice, curve, 1_000e6);
        uint256 owed = vault.owed(address(p), creator);
        assertGt(owed, 0);
        vm.prank(eve);
        vm.expectRevert(FeeVault.NothingToClaim.selector);
        vault.withdraw(address(p), eve);
        vm.prank(creator);
        vault.withdraw(address(p), creator);
        assertEq(p.balanceOf(creator), owed);
    }

    /* ----------------------------------------------------------------- Router */

    function test_router_callbackOnlyFromThePoolManager() public {
        vm.prank(eve);
        vm.expectRevert(Router.OnlyPoolManager.selector);
        router.unlockCallback("");
    }
}
