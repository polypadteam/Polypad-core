// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";
import {IUnlockCallback} from "v4-core/src/interfaces/callback/IUnlockCallback.sol";
import {BalanceDelta} from "v4-core/src/types/BalanceDelta.sol";
import {Currency} from "v4-core/src/types/Currency.sol";
import {PoolKey} from "v4-core/src/types/PoolKey.sol";
import {TickMath} from "v4-core/src/libraries/TickMath.sol";

import {BondingCurve} from "./BondingCurve.sol";
import {Graduator} from "./Graduator.sol";
import {PExchange} from "./PExchange.sol";
import {PriceOracle} from "./PriceOracle.sol";

/**
 * @title Router
 * @notice USDG in, coins out, in one transaction, and the reverse. The one
 *         address a terminal or indexer needs to watch for trades.
 *
 * buy:  USDG -> exchange mints pToken -> curve -> coins to the buyer
 * sell: coins -> curve -> pToken -> exchange burns it -> USDG to the seller
 *
 * `buy` / `sell` carry a signed quote from the pricer (our API, `/v1/swap`): the
 * live Polymarket book, sized to the trade. `sellForShares` needs no quote: it
 * sells coins for the pToken itself, a share of the market a holder can keep,
 * redeem later, or take to Polymarket.
 *
 * Every trade emits one `Swap` with its USDG amount, so price and volume need no
 * knowledge of pTokens: price per coin = usdg / coins. A `sellForShares` Swap
 * has usdg = 0 and names the pToken paid out.
 *
 * The same calls work before and after graduation: once a coin's curve has
 * graduated, the coin leg trades in its Uniswap v4 pool instead. A buy that
 * sells out the curve graduates it and spends the rest of its pToken in the new
 * pool in the same transaction. If graduation failed, that remainder goes back
 * to the buyer as pToken, which they can redeem.
 *
 * The router never holds a balance between calls.
 */
contract Router is ReentrancyGuard, IUnlockCallback {
    using SafeERC20 for IERC20;

    IERC20 public immutable usdg;
    PExchange public immutable exchange;
    IPoolManager public immutable poolManager;

    /**
     * @param usdg    USDG paid (buy, net of any pToken refund's value) or received (sell), 6 decimals
     * @param coins   coins received (buy) or sold (sell), 18 decimals
     * @param pTokens pToken that went into (buy) or came out of (sell) the curve, 6 decimals
     */
    event Swap(
        address indexed coin,
        address indexed trader,
        address indexed curve,
        bool isBuy,
        uint256 usdg,
        uint256 coins,
        uint256 pTokens,
        uint256 pTokenRefund
    );

    error OnlyPoolManager();
    error Slippage(uint256 got, uint256 minimum);
    error QuoteResult(uint256 amountOut);

    constructor(IERC20 usdg_, PExchange exchange_, IPoolManager poolManager_) {
        usdg = usdg_;
        exchange = exchange_;
        poolManager = poolManager_;
    }

    function buy(
        BondingCurve curve,
        uint256 usdgIn,
        uint256 minCoins,
        address to,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) external nonReentrant returns (uint256 coinsOut, uint256 pRefund) {
        IERC20 p = _pull(curve, usdgIn);
        uint256 pOut = exchange.mint(address(p), usdgIn, 0, address(this), q, sig);
        (coinsOut, pRefund) = _buy(curve, p, pOut, minCoins, to);
        emit Swap(address(curve.coin()), to, address(curve), true, usdgIn, coinsOut, pOut - pRefund, pRefund);
    }

    function sell(
        BondingCurve curve,
        uint256 coinsIn,
        uint256 minUsdg,
        address to,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) external nonReentrant returns (uint256 usdgOut) {
        (IERC20 c, IERC20 p, uint256 pOut) = _sellCoins(curve, coinsIn);
        usdgOut = exchange.redeem(address(p), pOut, minUsdg, to, q, sig);
        emit Swap(address(c), to, address(curve), false, usdgOut, coinsIn, pOut, 0);
    }

    /// @notice Sell coins for the market's pToken, paid to `to`. No quote, no USDG.
    function sellForShares(BondingCurve curve, uint256 coinsIn, uint256 minShares, address to)
        external
        nonReentrant
        returns (uint256 pOut)
    {
        IERC20 c;
        IERC20 p;
        (c, p, pOut) = _sellCoins(curve, coinsIn);
        if (pOut < minShares) revert Slippage(pOut, minShares);
        p.safeTransfer(to, pOut);
        emit Swap(address(c), to, address(curve), false, 0, coinsIn, pOut, 0);
    }

    /**
     * @notice Output of a trade in a graduated coin's pool, for quoting. Not a
     *         view (v4 simulates by swapping and reverting); call it with eth_call.
     * @param pTokenIn true: pToken in, coins out (a buy); false: coins in, pToken out
     */
    function quotePool(BondingCurve curve, bool pTokenIn, uint256 amountIn) external returns (uint256 amountOut) {
        PoolKey memory key = curve.graduator().poolKey(address(curve.coin()));
        bool zeroForOne = _zeroForOne(key, pTokenIn ? address(curve.pToken()) : address(curve.coin()));
        try poolManager.unlock(abi.encode(key, zeroForOne, amountIn, address(0), true)) {}
        catch (bytes memory err) {
            if (err.length != 36 || bytes4(err) != QuoteResult.selector) {
                assembly {
                    revert(add(err, 32), mload(err))
                }
            }
            assembly {
                amountOut := mload(add(err, 36))
            }
        }
    }

    /// @dev Exact-input swap inside the v4 lock. Pays the input from this contract.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != address(poolManager)) revert OnlyPoolManager();
        (PoolKey memory key, bool zeroForOne, uint256 amountIn, address to, bool quoteOnly) =
            abi.decode(data, (PoolKey, bool, uint256, address, bool));
        BalanceDelta delta = poolManager.swap(
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
        if (quoteOnly) revert QuoteResult(out);

        poolManager.sync(cin);
        IERC20(Currency.unwrap(cin)).safeTransfer(address(poolManager), uint256(uint128(-dIn)));
        poolManager.settle();
        poolManager.take(cout, to, out);
        return abi.encode(out);
    }

    /* ------------------------------------------------------------ internal */

    function _pull(BondingCurve curve, uint256 usdgIn) internal returns (IERC20 p) {
        p = curve.pToken();
        usdg.safeTransferFrom(msg.sender, address(this), usdgIn);
        usdg.forceApprove(address(exchange), usdgIn);
    }

    function _buy(BondingCurve curve, IERC20 p, uint256 pOut, uint256 minCoins, address to)
        internal
        returns (uint256 coinsOut, uint256 pRefund)
    {
        if (curve.graduated()) {
            coinsOut = _swap(curve, address(p), pOut, to);
        } else {
            p.forceApprove(address(curve), pOut);
            (coinsOut, pRefund) = curve.buy(pOut, 0, to);
            // This buy sold the curve out and graduated it: the rest buys in the pool.
            if (pRefund > 0 && curve.graduated()) {
                coinsOut += _swap(curve, address(p), pRefund, to);
                pRefund = 0;
            }
        }
        if (coinsOut < minCoins) revert Slippage(coinsOut, minCoins);
        if (pRefund > 0) p.safeTransfer(to, pRefund);
    }

    function _sellCoins(BondingCurve curve, uint256 coinsIn) internal returns (IERC20 c, IERC20 p, uint256 pOut) {
        c = curve.coin();
        p = curve.pToken();
        c.safeTransferFrom(msg.sender, address(this), coinsIn);
        if (curve.graduated()) {
            pOut = _swap(curve, address(c), coinsIn, address(this));
        } else {
            c.forceApprove(address(curve), coinsIn);
            pOut = curve.sell(coinsIn, 0, address(this));
        }
    }

    function _swap(BondingCurve curve, address tokenIn, uint256 amountIn, address to) internal returns (uint256) {
        PoolKey memory key = curve.graduator().poolKey(address(curve.coin()));
        bytes memory r = poolManager.unlock(abi.encode(key, _zeroForOne(key, tokenIn), amountIn, to, false));
        return abi.decode(r, (uint256));
    }

    function _zeroForOne(PoolKey memory key, address tokenIn) internal pure returns (bool) {
        return Currency.unwrap(key.currency0) == tokenIn;
    }
}
