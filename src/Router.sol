// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

import {BondingCurve} from "./BondingCurve.sol";
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
 * Two ways to price the share leg:
 * - `buy` / `sell` carry a signed quote from the pricer (our site, `/v1/swap`):
 *   the live book, sized to the trade.
 * - `buyPosted` / `sellPosted` use the oracle's posted price, so any contract or
 *   terminal can trade with plain calls. They pay a wider spread and are capped
 *   in size by the exchange.
 *
 * Every trade emits one `Swap` with its USDG amount, so price and volume need no
 * knowledge of pTokens: price per coin = usdg / coins.
 *
 * If a buy hits the end of the curve, the unused pToken goes to the buyer, who
 * can redeem it. The router never holds a balance between calls.
 */
contract Router is ReentrancyGuard {
    using SafeERC20 for IERC20;

    IERC20 public immutable usdg;
    PExchange public immutable exchange;

    /**
     * @param usdg    USDG paid (buy, net of any pToken refund's value) or received (sell), 6 decimals
     * @param coins   coins received (buy) or sold (sell), 18 decimals
     * @param pTokens pToken that went into (buy) or came out of (sell) the curve, 6 decimals
     * @param posted  true when priced by the posted price, false by a signed quote
     */
    event Swap(
        address indexed coin,
        address indexed trader,
        address indexed curve,
        bool isBuy,
        uint256 usdg,
        uint256 coins,
        uint256 pTokens,
        uint256 pTokenRefund,
        bool posted
    );

    constructor(IERC20 usdg_, PExchange exchange_) {
        usdg = usdg_;
        exchange = exchange_;
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
        emit Swap(address(curve.coin()), to, address(curve), true, usdgIn, coinsOut, pOut - pRefund, pRefund, false);
    }

    /// @notice `buy` at the posted price: no quote, wider spread, size capped by the exchange.
    function buyPosted(BondingCurve curve, uint256 usdgIn, uint256 minCoins, address to)
        external
        nonReentrant
        returns (uint256 coinsOut, uint256 pRefund)
    {
        IERC20 p = _pull(curve, usdgIn);
        uint256 pOut = exchange.mintPosted(address(p), usdgIn, 0, address(this));
        (coinsOut, pRefund) = _buy(curve, p, pOut, minCoins, to);
        emit Swap(address(curve.coin()), to, address(curve), true, usdgIn, coinsOut, pOut - pRefund, pRefund, true);
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
        emit Swap(address(c), to, address(curve), false, usdgOut, coinsIn, pOut, 0, false);
    }

    /// @notice `sell` at the posted price (or the payout once settled): no quote.
    function sellPosted(BondingCurve curve, uint256 coinsIn, uint256 minUsdg, address to)
        external
        nonReentrant
        returns (uint256 usdgOut)
    {
        (IERC20 c, IERC20 p, uint256 pOut) = _sellCoins(curve, coinsIn);
        usdgOut = exchange.redeemPosted(address(p), pOut, minUsdg, to);
        emit Swap(address(c), to, address(curve), false, usdgOut, coinsIn, pOut, 0, true);
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
        p.forceApprove(address(curve), pOut);
        (coinsOut, pRefund) = curve.buy(pOut, minCoins, to);
        if (pRefund > 0) p.safeTransfer(to, pRefund);
    }

    function _sellCoins(BondingCurve curve, uint256 coinsIn) internal returns (IERC20 c, IERC20 p, uint256 pOut) {
        c = curve.coin();
        p = curve.pToken();
        c.safeTransferFrom(msg.sender, address(this), coinsIn);
        c.forceApprove(address(curve), coinsIn);
        pOut = curve.sell(coinsIn, 0, address(this));
    }
}
