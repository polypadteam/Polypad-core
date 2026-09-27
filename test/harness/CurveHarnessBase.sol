// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";
import {IUnlockCallback} from "v4-core/src/interfaces/callback/IUnlockCallback.sol";
import {BalanceDelta} from "v4-core/src/types/BalanceDelta.sol";
import {Currency} from "v4-core/src/types/Currency.sol";
import {PoolKey} from "v4-core/src/types/PoolKey.sol";
import {TickMath} from "v4-core/src/libraries/TickMath.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";

import {PolypadBase} from "../Polypad.t.sol";

/// @dev Exact-input swaps straight on a v4 pool in raw tokens (no USDG leg), for
///      pool-level round trips. Leftover input, if any, is returned to the caller.
contract PoolSwapper is IUnlockCallback {
    IPoolManager internal immutable pm;

    constructor(IPoolManager pm_) {
        pm = pm_;
    }

    function swap(PoolKey memory key, address tokenIn, uint256 amountIn, address to) external returns (uint256 out) {
        IERC20(tokenIn).transferFrom(msg.sender, address(this), amountIn);
        out = abi.decode(pm.unlock(abi.encode(key, tokenIn, amountIn, to)), (uint256));
        uint256 left = IERC20(tokenIn).balanceOf(address(this));
        if (left > 0) IERC20(tokenIn).transfer(msg.sender, left);
    }

    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        require(msg.sender == address(pm), "pm");
        (PoolKey memory key, address tokenIn, uint256 amountIn, address to) =
            abi.decode(data, (PoolKey, address, uint256, address));
        bool zeroForOne = Currency.unwrap(key.currency0) == tokenIn;
        BalanceDelta delta = pm.swap(
            key,
            IPoolManager.SwapParams({
                zeroForOne: zeroForOne,
                amountSpecified: -int256(amountIn),
                sqrtPriceLimitX96: zeroForOne ? TickMath.MIN_SQRT_PRICE + 1 : TickMath.MAX_SQRT_PRICE - 1
            }),
            ""
        );
        (Currency cin, Currency cout) = zeroForOne ? (key.currency0, key.currency1) : (key.currency1, key.currency0);
        (int128 dIn, int128 dOut) = zeroForOne ? (delta.amount0(), delta.amount1()) : (delta.amount1(), delta.amount0());
        uint256 out = uint256(uint128(dOut));
        pm.sync(cin);
        IERC20(Currency.unwrap(cin)).transfer(address(pm), uint256(uint128(-dIn)));
        pm.settle();
        pm.take(cout, to, out);
        return abi.encode(out);
    }
}

/// @dev One launched coin on the 60c market, unlimited unbacked cap, funded actors,
///      and helpers that trade in raw pToken (no exchange spread noise) or via the Router.
abstract contract CurveHarnessBase is PolypadBase {
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;
    PoolSwapper internal swapper;

    address internal attacker = makeAddr("attacker");
    address internal victim = makeAddr("victim");
    address internal poster = makeAddr("poster");

    function setUp() public virtual override {
        super.setUp();
        (coin, curve) = _launch(ID);
        p = exchange.pTokenOf(ID);
        vm.startPrank(owner);
        exchange.setMaxUnbacked(ID, type(uint256).max);
        oracle.setPoster(poster);
        vm.stopPrank();
        swapper = new PoolSwapper(poolManager);
        // A float deep enough that no redemption here ever queues.
        usdg.mint(address(exchange), 10_000_000e6);
        _fund(attacker);
        _fund(victim);
        _fund(alice);
        _fund(bob);
        _postOnChain(ID, px[ID]);
    }

    function _fund(address who) internal {
        usdg.mint(who, 10_000_000e6);
        vm.startPrank(who);
        usdg.approve(address(router), type(uint256).max);
        usdg.approve(address(exchange), type(uint256).max);
        vm.stopPrank();
    }

    function _postOnChain(uint256 id, uint64 price) internal {
        uint256[] memory ids = new uint256[](1);
        uint64[] memory prices = new uint64[](1);
        ids[0] = id;
        prices[0] = price;
        vm.prank(poster);
        oracle.post(ids, prices);
    }

    /// @dev pToken for `who`, minted at the signed quote.
    function _shares(address who, uint256 usdgIn) internal returns (uint256 got) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(who);
        got = exchange.mint(address(p), usdgIn, 0, who, q, sig);
    }

    function _curveBuy(address who, uint256 pIn, uint256 minOut) internal returns (uint256 out, uint256 refund) {
        vm.startPrank(who);
        p.approve(address(curve), pIn);
        (out, refund) = curve.buy(pIn, minOut, who);
        vm.stopPrank();
    }

    function _curveSell(address who, uint256 coins, uint256 minOut) internal returns (uint256 out) {
        vm.startPrank(who);
        coin.approve(address(curve), coins);
        out = curve.sell(coins, minOut, who);
        vm.stopPrank();
    }

    /// @dev Raw pToken -> coin (or coin -> pToken) in the graduated pool.
    function _poolSwap(address who, address tokenIn, uint256 amountIn) internal returns (uint256 out) {
        PoolKey memory key = graduator.poolKey(address(coin));
        vm.startPrank(who);
        IERC20(tokenIn).approve(address(swapper), amountIn);
        out = swapper.swap(key, tokenIn, amountIn, who);
        vm.stopPrank();
    }

    /// @dev Sell the curve out with raw pToken from bob, refund returned to bob.
    function _graduateDirect() internal {
        uint256 pIn = _shares(bob, 8_000e6);
        _curveBuy(bob, pIn, 0);
        assertTrue(curve.graduated(), "graduated");
    }

    function _routerBuy(address who, uint256 usdgIn, uint256 minCoins) internal returns (uint256 coins, uint256 refund) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(who);
        return router.buy(curve, usdgIn, minCoins, who, q, sig);
    }

    function _routerSell(address who, uint256 coins, uint256 minUsdg) internal returns (uint256) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.startPrank(who);
        coin.approve(address(router), coins);
        uint256 out = router.sell(curve, coins, minUsdg, who, q, sig);
        vm.stopPrank();
        return out;
    }

    function _assertRouterEmpty() internal view {
        assertEq(usdg.balanceOf(address(router)), 0, "router usdg");
        assertEq(p.balanceOf(address(router)), 0, "router pToken");
        assertEq(coin.balanceOf(address(router)), 0, "router coin");
    }

    /// @dev The curve's solvency: selling every circulating curve coin never needs
    ///      more pToken than the curve holds.
    function _assertCurveSolvent() internal view {
        if (curve.graduated()) return;
        uint256 outstanding = coin.SUPPLY() - curve.trackedTokens();
        uint256 gross = (outstanding * (curve.phantom() + curve.trackedQuote())) / (curve.trackedTokens() + outstanding);
        assertLe(gross, curve.trackedQuote(), "curve insolvent");
        assertEq(p.balanceOf(address(curve)), curve.trackedQuote(), "tracked quote");
        assertEq(coin.balanceOf(address(curve)), curve.trackedTokens(), "tracked tokens");
    }
}
