// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

/**
 * @title BondingCurve
 * @notice One coin's launch curve, trading the coin against its market's pToken.
 *
 * The shape is Pons's (read from WORM's curve on chain): constant product over a
 * phantom quote reserve plus the real pToken deposited, with 2/7 of supply
 * (28.57%) held back for the pool. The phantom reserve is 0.4x the graduation
 * target, so selling the last curve token has raised exactly the target:
 *
 *     phantom x SUPPLY = (phantom + target) x reserved
 *     reserved = 2/7 SUPPLY  =>  target = 2.5 x phantom
 *
 * Because the quote asset is a Polymarket share, the coin's dollar price is its
 * price in shares times the share price. Demand moves the first factor, the
 * odds move the second.
 *
 * Reserves are tracked, never read from balances, so tokens sent here directly
 * cannot move the price.
 *
 * Phase 1 has no pool: when the sellable supply runs out, buys stop and sells
 * stay open. Graduation into Uniswap v4 comes in phase 2.
 *
 * Fees are 1% of the pToken side of every trade: 60% to the creator, 20% to
 * the exchange (burned there, so the desk sells the share and the proceeds
 * warm the Polymarket balance), 20% to the platform.
 */
contract BondingCurve is ReentrancyGuard {
    using SafeERC20 for IERC20;

    uint256 public constant BPS = 10_000;
    uint256 public constant FEE_BPS = 100;
    uint256 public constant CREATOR_SHARE_BPS = 6_000;
    uint256 public constant FLOAT_SHARE_BPS = 2_000;

    IERC20 public immutable pToken;
    address public immutable exchange;
    address public immutable creator;
    address public immutable platform;
    address public immutable factory;
    /// @notice Virtual pToken reserve, 6 decimals. Never held.
    uint256 public immutable phantom;

    IERC20 public coin;
    /// @notice Coins held back for the pool. Never sold by the curve.
    uint256 public reserved;
    /// @notice Real pToken deposited by buyers, net of fees.
    uint256 public trackedQuote;
    /// @notice Coins the curve holds, reserved included.
    uint256 public trackedTokens;

    event Buy(address indexed buyer, address indexed to, uint256 quoteIn, uint256 coinsOut, uint256 fee, uint256 refund);
    event Sell(address indexed seller, address indexed to, uint256 coinsIn, uint256 quoteOut, uint256 fee);
    event SoldOut();

    error OnlyFactory();
    error AlreadyInitialized();
    error NotInitialized();
    error ZeroAmount();
    error SoldOut_();
    error Slippage(uint256 got, uint256 minimum);

    constructor(IERC20 pToken_, address exchange_, address creator_, address platform_, uint256 phantom_) {
        pToken = pToken_;
        exchange = exchange_;
        creator = creator_;
        platform = platform_;
        factory = msg.sender;
        phantom = phantom_;
    }

    /// @notice Wire the coin once it exists; the coin mints its whole supply to this contract.
    function initialize(IERC20 coin_) external {
        if (msg.sender != factory) revert OnlyFactory();
        if (address(coin) != address(0)) revert AlreadyInitialized();
        coin = coin_;
        trackedTokens = coin_.balanceOf(address(this));
        reserved = (trackedTokens * 2) / 7;
    }

    /* --------------------------------------------------------------- trade */

    /**
     * @notice Spend pToken, receive coins. If the curve would sell past its
     *         reserve, the buy is filled up to the reserve and the unused pToken
     *         is refunded to the caller.
     */
    function buy(uint256 quoteIn, uint256 minOut, address to)
        external
        nonReentrant
        returns (uint256 out, uint256 refund)
    {
        if (address(coin) == address(0)) revert NotInitialized();
        if (quoteIn == 0) revert ZeroAmount();
        uint256 sellable = trackedTokens - reserved;
        if (sellable == 0) revert SoldOut_();

        uint256 fee = (quoteIn * FEE_BPS) / BPS;
        uint256 net = quoteIn - fee;
        uint256 v = phantom + trackedQuote;
        out = (net * trackedTokens) / (v + net);

        if (out >= sellable) {
            out = sellable;
            // Net pToken that buys exactly `out`, rounded up, and its fee.
            net = (v * out + (trackedTokens - out) - 1) / (trackedTokens - out);
            if (net > quoteIn) net = quoteIn;
            fee = (net * FEE_BPS + (BPS - FEE_BPS) - 1) / (BPS - FEE_BPS);
            if (net + fee > quoteIn) fee = quoteIn - net;
            refund = quoteIn - net - fee;
            emit SoldOut();
        }
        if (out == 0) revert ZeroAmount();
        if (out < minOut) revert Slippage(out, minOut);

        trackedQuote += net;
        trackedTokens -= out;

        pToken.safeTransferFrom(msg.sender, address(this), quoteIn - refund);
        coin.safeTransfer(to, out);
        _payFee(fee);
        emit Buy(msg.sender, to, quoteIn - refund, out, fee, refund);
    }

    /// @notice Sell coins back to the curve for pToken.
    function sell(uint256 coinsIn, uint256 minOut, address to) external nonReentrant returns (uint256 out) {
        if (address(coin) == address(0)) revert NotInitialized();
        if (coinsIn == 0) revert ZeroAmount();

        uint256 v = phantom + trackedQuote;
        uint256 gross = (coinsIn * v) / (trackedTokens + coinsIn);
        // Rounding keeps phantom + trackedQuote above the invariant, so this holds;
        // the clamp is defensive against a curve that was never bought.
        if (gross > trackedQuote) gross = trackedQuote;
        uint256 fee = (gross * FEE_BPS) / BPS;
        out = gross - fee;
        if (out == 0) revert ZeroAmount();
        if (out < minOut) revert Slippage(out, minOut);

        trackedQuote -= gross;
        trackedTokens += coinsIn;

        coin.safeTransferFrom(msg.sender, address(this), coinsIn);
        pToken.safeTransfer(to, out);
        _payFee(fee);
        emit Sell(msg.sender, to, coinsIn, out, fee);
    }

    /* --------------------------------------------------------------- views */

    function quoteBuy(uint256 quoteIn) external view returns (uint256 out) {
        uint256 net = quoteIn - (quoteIn * FEE_BPS) / BPS;
        out = (net * trackedTokens) / (phantom + trackedQuote + net);
        uint256 sellable = trackedTokens - reserved;
        if (out > sellable) out = sellable;
    }

    function quoteSell(uint256 coinsIn) external view returns (uint256 out) {
        uint256 gross = (coinsIn * (phantom + trackedQuote)) / (trackedTokens + coinsIn);
        if (gross > trackedQuote) gross = trackedQuote;
        out = gross - (gross * FEE_BPS) / BPS;
    }

    /// @notice Marginal price in pToken per whole coin, 6 decimals (pToken units per 1e18 coin).
    function spotPrice() external view returns (uint256) {
        return ((phantom + trackedQuote) * 1e18) / trackedTokens;
    }

    /// @notice pToken raised when the curve sells out.
    function target() external view returns (uint256) {
        return (phantom * 5) / 2;
    }

    function soldOut() external view returns (bool) {
        return trackedTokens == reserved;
    }

    /* ------------------------------------------------------------ internal */

    function _payFee(uint256 fee) internal {
        if (fee == 0) return;
        uint256 toCreator = (fee * CREATOR_SHARE_BPS) / BPS;
        uint256 toFloat = (fee * FLOAT_SHARE_BPS) / BPS;
        uint256 toPlatform = fee - toCreator - toFloat;
        if (toCreator > 0) pToken.safeTransfer(creator, toCreator);
        if (toFloat > 0) pToken.safeTransfer(exchange, toFloat);
        if (toPlatform > 0) pToken.safeTransfer(platform, toPlatform);
    }
}
