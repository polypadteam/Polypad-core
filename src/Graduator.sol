// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";
import {IHooks} from "v4-core/src/interfaces/IHooks.sol";
import {IUnlockCallback} from "v4-core/src/interfaces/callback/IUnlockCallback.sol";
import {BalanceDelta} from "v4-core/src/types/BalanceDelta.sol";
import {Currency} from "v4-core/src/types/Currency.sol";
import {PoolKey} from "v4-core/src/types/PoolKey.sol";
import {PoolId} from "v4-core/src/types/PoolId.sol";
import {FullMath} from "v4-core/src/libraries/FullMath.sol";
import {FixedPoint96} from "v4-core/src/libraries/FixedPoint96.sol";
import {TickMath} from "v4-core/src/libraries/TickMath.sol";
import {StateLibrary} from "v4-core/src/libraries/StateLibrary.sol";
import {PoolIdLibrary} from "v4-core/src/types/PoolId.sol";

/**
 * @title Graduator
 * @notice Moves a sold-out curve into a Uniswap v4 pool and owns that pool's
 *         liquidity forever, paying its trading fees to the creator and platform.
 *
 * Each graduated coin gets one ordinary v4 pool against its market's pToken:
 * the LP fee fixed at the coin's launch (1% by default), tick spacing 200, one
 * full-range position. The position is never removed, so the liquidity is
 * locked. Fees accrue to the position and anyone can `collect` them: the coin's
 * creator share to its FeeVault, the rest to the platform, in both tokens.
 *
 * This contract is also the pools' hook, with one permission: `beforeInitialize`,
 * which lets only this contract create a Polypad pool. Without it anyone could
 * initialise a coin's pool at a bad price before graduation. Its address is
 * mined so that its low 14 bits are exactly `BEFORE_INITIALIZE_FLAG` (0x2000);
 * v4 reads a hook's permissions from its address.
 *
 * The pool starts at the curve's final price: the curve passes its whole raise
 * and just enough of its reserved coins to match that price, and burns the rest.
 */
contract Graduator is IUnlockCallback, ReentrancyGuard {
    using SafeERC20 for IERC20;
    using PoolIdLibrary for PoolKey;

    int24 public constant TICK_SPACING = 200;
    uint256 public constant BPS = 10_000;

    IPoolManager public immutable poolManager;
    /// @notice The factory whose curves may graduate here.
    address public immutable factory;

    struct Pool {
        PoolKey key;
        address feeVault;
        address platform;
        uint128 liquidity;
        uint16 creatorShareBps;
    }

    mapping(address coin => Pool) internal pools;

    event Graduated(
        address indexed coin,
        address indexed pToken,
        bytes32 indexed poolId,
        uint256 coins,
        uint256 pTokens,
        uint128 liquidity
    );
    event FeesCollected(address indexed coin, uint256 fee0, uint256 fee1);

    error OnlyPoolManager();
    error OnlyCurve();
    error OnlySelf();
    error AlreadyGraduated(address coin);
    error NotGraduated(address coin);

    constructor(IPoolManager poolManager_, address factory_) {
        poolManager = poolManager_;
        factory = factory_;
    }

    /* ----------------------------------------------------------- graduate */

    /**
     * @notice Called by a factory curve at sell-out. Pulls `coins` and `pTokens`
     *         from the curve (which approved them) into a new full-range pool
     *         priced at `pTokens / coins`. Dust that does not fit the position
     *         goes to the platform.
     */
    function graduate(
        IERC20 coin,
        IERC20 pToken,
        uint256 coins,
        uint256 pTokens,
        address feeVault,
        address platform,
        uint24 fee,
        uint16 creatorShareBps
    ) external nonReentrant returns (bytes32 poolId) {
        if (!IsCurve(factory).isCurve(msg.sender)) revert OnlyCurve();
        if (pools[address(coin)].liquidity != 0) revert AlreadyGraduated(address(coin));

        coin.safeTransferFrom(msg.sender, address(this), coins);
        pToken.safeTransferFrom(msg.sender, address(this), pTokens);

        PoolKey memory key = _key(address(coin), address(pToken), fee);
        uint128 liquidity = address(coin) < address(pToken) ? _seed(key, coins, pTokens) : _seed(key, pTokens, coins);
        pools[address(coin)] = Pool(key, feeVault, platform, liquidity, creatorShareBps);

        // Rounding leaves a few units behind; they are not worth a second position.
        uint256 coinDust = coin.balanceOf(address(this));
        uint256 pDust = pToken.balanceOf(address(this));
        if (coinDust > 0) coin.safeTransfer(platform, coinDust);
        if (pDust > 0) pToken.safeTransfer(platform, pDust);

        poolId = PoolId.unwrap(key.toId());
        emit Graduated(address(coin), address(pToken), poolId, coins - coinDust, pTokens - pDust, liquidity);
    }

    /* ------------------------------------------------------------ fees */

    /// @notice Pay a graduated coin's accrued pool fees: its creator share to its FeeVault, the rest to the platform.
    function collect(address coin) external nonReentrant returns (uint256 fee0, uint256 fee1) {
        Pool storage p = pools[coin];
        if (p.liquidity == 0) revert NotGraduated(coin);
        poolManager.unlock(abi.encode(p.key, uint128(0)));

        IERC20 t0 = IERC20(Currency.unwrap(p.key.currency0));
        IERC20 t1 = IERC20(Currency.unwrap(p.key.currency1));
        fee0 = t0.balanceOf(address(this));
        fee1 = t1.balanceOf(address(this));
        _split(coin, t0, fee0, p);
        _split(coin, t1, fee1, p);
        emit FeesCollected(coin, fee0, fee1);
    }

    /* ----------------------------------------------------------- v4 hooks */

    /// @dev Adds the position (liquidity > 0) or pokes it to realise fees (liquidity == 0).
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != address(poolManager)) revert OnlyPoolManager();
        (PoolKey memory key, uint128 liquidity) = abi.decode(data, (PoolKey, uint128));
        (int24 lower, int24 upper) = _range();
        (BalanceDelta delta,) = poolManager.modifyLiquidity(
            key,
            IPoolManager.ModifyLiquidityParams({
                tickLower: lower, tickUpper: upper, liquidityDelta: int256(uint256(liquidity)), salt: bytes32(0)
            }),
            ""
        );
        _net(key.currency0, delta.amount0());
        _net(key.currency1, delta.amount1());
        return "";
    }

    /// @notice Only this contract may create a pool that uses it as its hook.
    function beforeInitialize(address sender, PoolKey calldata, uint160) external view returns (bytes4) {
        if (msg.sender != address(poolManager)) revert OnlyPoolManager();
        if (sender != address(this)) revert OnlySelf();
        return IHooks.beforeInitialize.selector;
    }

    /* --------------------------------------------------------------- views */

    function poolKey(address coin) external view returns (PoolKey memory) {
        Pool storage p = pools[coin];
        if (p.liquidity == 0) revert NotGraduated(coin);
        return p.key;
    }

    function poolOf(address coin)
        external
        view
        returns (bytes32 poolId, address feeVault, address platform, uint128 liquidity)
    {
        Pool storage p = pools[coin];
        return (PoolId.unwrap(p.key.toId()), p.feeVault, p.platform, p.liquidity);
    }

    /// @notice Pool price in pToken per whole coin, 6 decimals (same unit as a curve's spotPrice).
    function spotPrice(address coin) external view returns (uint256) {
        Pool storage p = pools[coin];
        if (p.liquidity == 0) revert NotGraduated(coin);
        (uint160 sqrtP,,,) = StateLibrary.getSlot0(poolManager, p.key.toId());
        if (Currency.unwrap(p.key.currency0) == coin) {
            // pToken per coin = sqrtP^2 / 2^192
            return FullMath.mulDiv(FullMath.mulDiv(sqrtP, sqrtP, 1 << 64), 1e18, 1 << 128);
        }
        // coin is currency1: pToken per coin = 2^192 / sqrtP^2
        return FullMath.mulDiv(FullMath.mulDiv(1e18, 1 << 96, sqrtP), 1 << 96, sqrtP);
    }

    function graduated(address coin) external view returns (bool) {
        return pools[coin].liquidity != 0;
    }

    /* ------------------------------------------------------------ internal */

    function _key(address coin, address pToken, uint24 fee) internal view returns (PoolKey memory) {
        bool coinIs0 = coin < pToken;
        return PoolKey({
            currency0: Currency.wrap(coinIs0 ? coin : pToken),
            currency1: Currency.wrap(coinIs0 ? pToken : coin),
            fee: fee,
            tickSpacing: TICK_SPACING,
            hooks: IHooks(address(this))
        });
    }

    /// @dev Create the pool at amount1/amount0 and add one full-range position from these amounts.
    function _seed(PoolKey memory key, uint256 amount0, uint256 amount1) internal returns (uint128 liquidity) {
        uint160 sqrtPrice = uint160(Math.sqrt(FullMath.mulDiv(amount1, 1 << 192, amount0)));
        poolManager.initialize(key, sqrtPrice);
        (int24 lower, int24 upper) = _range();
        liquidity = _liquidity(
            sqrtPrice, TickMath.getSqrtPriceAtTick(lower), TickMath.getSqrtPriceAtTick(upper), amount0, amount1
        );
        poolManager.unlock(abi.encode(key, liquidity));
    }

    function _range() internal pure returns (int24 lower, int24 upper) {
        upper = (TickMath.MAX_TICK / TICK_SPACING) * TICK_SPACING;
        lower = -upper;
    }

    /// @dev Pay what the pool is owed (negative delta) or take what it owes us (positive).
    function _net(Currency c, int128 amount) internal {
        if (amount < 0) {
            poolManager.sync(c);
            IERC20(Currency.unwrap(c)).safeTransfer(address(poolManager), uint256(uint128(-amount)));
            poolManager.settle();
        } else if (amount > 0) {
            poolManager.take(c, address(this), uint256(uint128(amount)));
        }
    }

    function _split(address coin, IERC20 t, uint256 amount, Pool storage p) internal {
        if (amount == 0) return;
        uint256 toCreator = (amount * p.creatorShareBps) / BPS;
        if (toCreator > 0) {
            t.forceApprove(p.feeVault, toCreator);
            IFeeVault(p.feeVault).deposit(coin, address(t), toCreator);
        }
        if (amount > toCreator) t.safeTransfer(p.platform, amount - toCreator);
    }

    /// @dev Liquidity for both amounts at `sqrtP` over [sqrtA, sqrtB], the smaller of the two.
    function _liquidity(uint160 sqrtP, uint160 sqrtA, uint160 sqrtB, uint256 amount0, uint256 amount1)
        internal
        pure
        returns (uint128)
    {
        uint256 l0 = FullMath.mulDiv(amount0, FullMath.mulDiv(sqrtP, sqrtB, FixedPoint96.Q96), sqrtB - sqrtP);
        uint256 l1 = FullMath.mulDiv(amount1, FixedPoint96.Q96, sqrtP - sqrtA);
        uint256 l = l0 < l1 ? l0 : l1;
        // Leave a hair for rounding inside the pool so the settle never falls short.
        return uint128(l - l / 1e9 - 1);
    }
}

interface IFeeVault {
    function deposit(address coin, address asset, uint256 amount) external;
}

interface IsCurve {
    function isCurve(address) external view returns (bool);
}
