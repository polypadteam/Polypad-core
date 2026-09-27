// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

import {PExchange} from "./PExchange.sol";
import {PriceOracle} from "./PriceOracle.sol";

/**
 * @title FeeVault
 * @notice Where every coin's creator fees go: the creator's claimable balance,
 *         and, for coins launched with a holder share, a dividend that streams
 *         to the coin's holders.
 *
 * At launch the creator picks `holdersBps`, the part of their fee paid to
 * holders (0 to 100%). It is fixed for the life of the coin, so holders can
 * rely on it. Fees arrive from the curve on every trade and from the pool when
 * its fees are collected, in pToken and (pool sells) in the coin itself:
 *
 * - The creator's part is credited to the coin's payee, claimable any time as
 *   pToken or cashed out to USDG through the exchange (`withdraw`, `withdrawUsd`).
 * - The holders' part in pToken streams to holders over `STREAM` (1 hour).
 * - The holders' part in coins is burned, which every holder shares pro rata.
 *
 * Holder accounting is the masterchef accumulator from arc-cafe's CafeRewards
 * (audited there): a holder earns `balance x (acc_now - acc_at_last_settle)`,
 * and every balance change settles first, because the coin calls
 * `onTransfer` after each transfer. The pool (the v4 PoolManager), the curve,
 * the Graduator, the burn address and this vault never earn.
 *
 * Why a stream instead of paying each fee out at once: pool fees are realised
 * only when someone calls `collect`, so a lump distributed at that instant could
 * be captured by buying just before the call and selling just after. Streamed
 * over an hour, a holder earns only for the time they actually hold. Curve fees
 * go through the same stream for the same reason.
 *
 * Payout is pull-based, and anyone may push it: `claimFor` pays a list of
 * holders what they are owed, in pToken or converted to USDG with a pricer
 * quote. Our keeper runs it on a schedule, so holders are paid without doing
 * anything. A holder the payment cannot reach is skipped, not reverted, so one
 * address cannot block a batch.
 */
contract FeeVault is ReentrancyGuard {
    using SafeERC20 for IERC20;

    uint256 public constant BPS = 10_000;
    /// @notice How long a deposit for holders takes to stream out.
    uint256 public constant STREAM = 1 hours;
    /// @dev Accumulator scale. Coin supply is 1e27, pToken amounts fit in 1e15 for
    ///      any realistic fee, so a step is at most 1e15 x 1e36 / MIN_SUPPLY = 1e33.
    uint256 public constant ACC = 1e36;
    /// @dev balance x acc must never overflow: balance <= 1e27 (the coin's supply).
    uint256 public constant MAX_ACC = type(uint256).max / 1_000_000_000e18;
    /// @notice Below 0.01% of supply held by wallets, nothing streams (it waits),
    ///         so the last dust holder of a dead coin cannot collect the stream.
    uint256 public constant MIN_SUPPLY = 100_000e18;
    address public constant DEAD = 0x000000000000000000000000000000000000dEaD;

    address public immutable factory;
    PExchange public immutable exchange;
    /// @notice Every v4 pool holds its coins here; it must never earn.
    address public immutable poolManager;

    struct Launch {
        address curve;
        address graduator;
        address pToken;
        address payee;
        uint16 holdersBps;
    }

    /// @dev Holder books for one coin.
    struct Book {
        uint256 acc; // pToken per coin, scaled by ACC; only rises
        uint256 supply; // sum of tracked holder balances
        uint256 pot; // released to holders and not yet paid
        uint256 streaming; // deposited for holders, not yet released
        uint64 last; // last release
        uint64 end; // stream end
    }

    mapping(address coin => Launch) public launches;
    mapping(address coin => Book) public books;
    mapping(address coin => mapping(address holder => uint256)) public tracked;
    mapping(address coin => mapping(address holder => uint256)) public debt;
    mapping(address coin => mapping(address holder => uint256)) public rewards;
    /// @notice Creator fees claimable by a payee, per asset (a pToken or a coin).
    mapping(address asset => mapping(address payee => uint256)) public owed;

    event Registered(address indexed coin, address indexed payee, address pToken, uint16 holdersBps);
    event PayeeSet(address indexed coin, address indexed payee);
    event Deposited(address indexed coin, address indexed asset, uint256 toPayee, uint256 toHolders);
    event Released(address indexed coin, uint256 amount, uint256 supply);
    event Withdrawn(address indexed asset, address indexed payee, address to, uint256 amount, uint256 usdgOut);
    event HolderPaid(address indexed coin, address indexed holder, uint256 amount, bool cashedOut);
    event HolderSkipped(address indexed coin, address indexed holder, uint256 amount);

    error OnlyFactory();
    error OnlyPayee();
    error Unknown(address coin);
    error BadAsset(address asset);
    error BadParams();
    error NothingToClaim();

    constructor(address factory_, PExchange exchange_, address poolManager_) {
        factory = factory_;
        exchange = exchange_;
        poolManager = poolManager_;
    }

    /* ------------------------------------------------------------ launches */

    /// @notice Called by the factory for each new coin.
    function register(
        address coin,
        address curve,
        address graduator,
        address pToken,
        address creator,
        uint16 holdersBps
    ) external {
        if (msg.sender != factory) revert OnlyFactory();
        if (holdersBps > BPS || launches[coin].curve != address(0)) revert BadParams();
        launches[coin] = Launch(curve, graduator, pToken, creator, holdersBps);
        emit Registered(coin, creator, pToken, holdersBps);
    }

    /// @notice Hand the creator's claim on a coin's future fees to another address.
    ///         What is already owed stays claimable by the old payee.
    function setPayee(address coin, address payee) external {
        Launch storage l = launches[coin];
        if (msg.sender != l.payee) revert OnlyPayee();
        if (payee == address(0)) revert BadParams();
        l.payee = payee;
        emit PayeeSet(coin, payee);
    }

    /**
     * @notice Pay in creator fees for `coin`, in its pToken or in the coin. Pulls
     *         `amount` from the caller. Curves and the Graduator call this; anyone
     *         else calling it is making a donation.
     */
    function deposit(address coin, address asset, uint256 amount) external {
        Launch storage l = launches[coin];
        if (l.curve == address(0)) revert Unknown(coin);
        if (asset != l.pToken && asset != coin) revert BadAsset(asset);
        if (amount == 0) return;
        IERC20(asset).safeTransferFrom(msg.sender, address(this), amount);

        uint256 toHolders = (amount * l.holdersBps) / BPS;
        uint256 toPayee = amount - toHolders;
        if (toPayee > 0) owed[asset][l.payee] += toPayee;
        if (toHolders > 0) {
            if (asset == coin) {
                IERC20(coin).safeTransfer(DEAD, toHolders);
            } else {
                _release(coin);
                Book storage b = books[coin];
                // The new amount streams over a full STREAM; what is still streaming
                // keeps its own pace. The end is their amount-weighted average, so a
                // stream of tiny deposits cannot hold back what is already there.
                uint256 left = b.end > block.timestamp ? b.end - block.timestamp : 0;
                uint256 total = b.streaming + toHolders;
                b.end = uint64(block.timestamp + (b.streaming * left + toHolders * STREAM) / total);
                b.streaming = total;
            }
        }
        emit Deposited(coin, asset, toPayee, toHolders);
    }

    /* ----------------------------------------------------------- creators */

    /// @notice Claim creator fees owed to the caller in `asset`.
    function withdraw(address asset, address to) external nonReentrant returns (uint256 amount) {
        amount = owed[asset][msg.sender];
        if (amount == 0) revert NothingToClaim();
        owed[asset][msg.sender] = 0;
        IERC20(asset).safeTransfer(to, amount);
        emit Withdrawn(asset, msg.sender, to, amount, 0);
    }

    /**
     * @notice Claim creator fees owed in a pToken as USDG: the exchange buys the
     *         shares back at a pricer SELL quote (or at the payout once the market
     *         has settled; the quote is then ignored).
     */
    function withdrawUsd(address pToken, address to, uint256 minOut, PriceOracle.Quote calldata q, bytes calldata sig)
        external
        nonReentrant
        returns (uint256 out)
    {
        uint256 amount = owed[pToken][msg.sender];
        if (amount == 0) revert NothingToClaim();
        owed[pToken][msg.sender] = 0;
        out = exchange.redeem(pToken, amount, minOut, to, q, sig);
        emit Withdrawn(pToken, msg.sender, to, amount, out);
    }

    /* ------------------------------------------------------------ holders */

    /// @notice Called by a holder-share coin after every transfer.
    /// @dev Not nonReentrant: it runs inside other contracts' transfers, including
    ///      this vault's own. It makes no external call but `balanceOf` on the coin.
    function onTransfer(address from, address to) external {
        if (launches[msg.sender].holdersBps == 0) return; // not one of ours
        _release(msg.sender);
        _settle(msg.sender, from);
        _settle(msg.sender, to);
    }

    /// @notice Claim your holder rewards on `coin`, in its pToken.
    function claim(address coin, address to) external nonReentrant returns (uint256 amount) {
        address p = launches[coin].pToken;
        if (p == address(0)) revert Unknown(coin);
        _release(coin);
        _settle(coin, msg.sender);
        amount = _debit(coin, msg.sender);
        if (amount == 0) revert NothingToClaim();
        IERC20(p).safeTransfer(to, amount);
        emit HolderPaid(coin, msg.sender, amount, false);
    }

    /**
     * @notice Pay `holders` what they are owed on `coin`. Anyone may call; each
     *         holder is paid only their own rewards, to their own address.
     * @param minAmount skip holders owed less (not worth the gas)
     * @param cashOut pay USDG through the exchange at quote `q` instead of pToken;
     *        honoured only for a holder claiming their own rewards.
     *        A redemption the exchange refuses (halted, outflow cap) leaves the
     *        holder's rewards untouched.
     */
    function claimFor(
        address coin,
        address[] calldata holders,
        uint256 minAmount,
        bool cashOut,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) external nonReentrant returns (uint256 paid) {
        address p = launches[coin].pToken;
        if (p == address(0)) revert Unknown(coin);
        _release(coin);
        for (uint256 i; i < holders.length; ++i) {
            paid += _payOne(coin, p, holders[i], minAmount, cashOut, q, sig);
        }
    }

    function _payOne(
        address coin,
        address p,
        address h,
        uint256 minAmount,
        bool cashOut,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) internal returns (uint256) {
        _settle(coin, h);
        uint256 amount = Math.min(rewards[coin][h], books[coin].pot);
        if (amount == 0 || amount < minAmount) return 0;
        _debit(coin, h);
        // Only a holder may turn their own rewards into USDG: nobody can force a
        // sale (and its spread) on someone else.
        cashOut = cashOut && h == msg.sender;
        if (cashOut ? _tryRedeem(p, amount, h, q, sig) : _tryTransfer(p, h, amount)) {
            emit HolderPaid(coin, h, amount, cashOut);
            return amount;
        }
        // Credit back against the books as they are now.
        rewards[coin][h] += amount;
        books[coin].pot += amount;
        emit HolderSkipped(coin, h, amount);
        return 0;
    }

    /* --------------------------------------------------------------- views */

    /// @notice Holder rewards `user` could claim on `coin` now, in pToken.
    function pending(address coin, address user) external view returns (uint256) {
        if (_excluded(coin, user)) return 0;
        Book storage b = books[coin];
        (uint256 acc, uint256 released) = _preview(b);
        uint256 owedNow = rewards[coin][user] + (tracked[coin][user] * acc - debt[coin][user]) / ACC;
        uint256 pot = b.pot + released;
        return owedNow > pot ? pot : owedNow;
    }

    /* ------------------------------------------------------------ internal */

    function _excluded(address coin, address user) internal view returns (bool) {
        Launch storage l = launches[coin];
        return user == address(0) || user == DEAD || user == address(this) || user == poolManager || user == l.curve
            || user == l.graduator;
    }

    /// @dev Bank what `user` earned on the balance they held since their last
    ///      settle, then track their live balance.
    function _settle(address coin, address user) internal {
        if (_excluded(coin, user)) return;
        Book storage b = books[coin];
        uint256 acc = b.acc;
        uint256 t = tracked[coin][user];
        uint256 earned = (t * acc - debt[coin][user]) / ACC;
        if (earned > 0) rewards[coin][user] += earned;
        uint256 bal = IERC20(coin).balanceOf(user);
        if (bal != t) {
            b.supply = b.supply - t + bal;
            tracked[coin][user] = bal;
        }
        debt[coin][user] = bal * acc;
    }

    /// @dev Release the stream up to now across the holders tracked right now.
    function _release(address coin) internal {
        Book storage b = books[coin];
        if (b.last == block.timestamp) return;
        uint256 amount = _releasable(b);
        b.last = uint64(block.timestamp);
        if (amount == 0) return;
        if (b.supply < MIN_SUPPLY) {
            // Nobody to pay: hold it and stream it again once there are holders.
            b.end = uint64(block.timestamp + STREAM);
            return;
        }
        uint256 step = Math.mulDiv(amount, ACC, b.supply);
        uint256 room = MAX_ACC - b.acc;
        if (step > room) step = room;
        if (step == 0) return;
        b.acc += step;
        // Holders are owed at most floor(step x supply / ACC) between them, and the
        // pot takes the ceiling, so the pot always covers every holder in full.
        // step was floored, so this never exceeds `amount`. Dust stays streaming.
        uint256 applied = Math.mulDiv(step, b.supply, ACC, Math.Rounding.Ceil);
        b.streaming -= applied;
        b.pot += applied;
        emit Released(coin, applied, b.supply);
    }

    function _releasable(Book storage b) internal view returns (uint256) {
        if (b.streaming == 0) return 0;
        if (block.timestamp >= b.end || b.last == 0) return b.streaming;
        return (b.streaming * (block.timestamp - b.last)) / (b.end - b.last);
    }

    /// @dev What `_release` would do now, for views.
    function _preview(Book storage b) internal view returns (uint256 acc, uint256 released) {
        acc = b.acc;
        if (b.last == block.timestamp || b.supply < MIN_SUPPLY) return (acc, 0);
        uint256 amount = _releasable(b);
        uint256 step = Math.mulDiv(amount, ACC, b.supply);
        uint256 room = MAX_ACC - acc;
        if (step > room) step = room;
        return (acc + step, Math.mulDiv(step, b.supply, ACC, Math.Rounding.Ceil));
    }

    /// @dev Take `holder`'s rewards off the books, capped by the pot (the
    ///      accumulator can promise a wei or two more than was released).
    function _debit(address coin, address holder) internal returns (uint256 amount) {
        Book storage b = books[coin];
        amount = Math.min(rewards[coin][holder], b.pot);
        rewards[coin][holder] -= amount;
        b.pot -= amount;
    }

    function _tryTransfer(address token, address to, uint256 amount) internal returns (bool) {
        (bool ok, bytes memory data) = token.call(abi.encodeCall(IERC20.transfer, (to, amount)));
        return ok && (data.length == 0 || abi.decode(data, (bool)));
    }

    function _tryRedeem(address pToken, uint256 amount, address to, PriceOracle.Quote calldata q, bytes calldata sig)
        internal
        returns (bool)
    {
        try exchange.redeem(pToken, amount, 0, to, q, sig) returns (uint256) {
            return true;
        } catch {
            return false;
        }
    }
}
