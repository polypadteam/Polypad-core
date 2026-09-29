// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";

/**
 * @title PriceOracle
 * @notice Signed Polymarket prices, plus each market's pause and settlement.
 *
 * The pricer reads the live Polymarket book and signs a `Quote` when a user asks
 * to trade; the quote rides inside the user's own transaction and the exchange
 * checks it here. A quote is only seconds old when it is used and is sized to
 * the trade.
 *
 * A quote is one-sided. A BUY quote carries the price the desk would pay for a
 * share on Polymarket (the book walked for the trade's size; refused past
 * mid + 1c); a SELL quote carries the price it would get (the bid side walked
 * for the size). Users trade at what the desk can
 * actually match, so the float does not lose the bid/ask gap on every trade.
 * `maxAmount` bounds the trade the price was computed for: USDG for a buy,
 * pToken for a sell.
 *
 * Prices use six decimals: 600_000 is 60 cents.
 *
 * `paused` stops new buys only (holders can still sell). The keeper sets it
 * before a market's end date. `settle` records the final payout once the market
 * resolves on Polygon; it is one-way, and a settled market ignores quotes.
 *
 * No quote may price a share above `MAX_PRICE` (99c), on either side: a sale
 * signed near $1 by a stolen key would pay out almost a dollar for a share the
 * desk may never have held.
 */
contract PriceOracle is Ownable2Step, EIP712 {
    uint64 public constant ONE = 1e6;
    /// @notice Longest a quote may be valid for, whatever the signer wrote.
    uint64 public constant MAX_VALIDITY = 30;
    /// @notice Highest price any quote may carry.
    uint64 public constant MAX_PRICE = 990_000;

    uint8 public constant BUY = 0;
    uint8 public constant SELL = 1;

    bytes32 public constant QUOTE_TYPEHASH =
        keccak256("Quote(uint256 positionId,uint8 side,uint64 price,uint256 maxAmount,uint64 validUntil)");

    struct Quote {
        uint256 positionId;
        uint8 side;
        uint64 price;
        uint256 maxAmount;
        uint64 validUntil;
    }

    struct Status {
        bool paused;
        /// @dev Whether it was paused before a settlement was recorded, to restore on cancel.
        bool pausedBeforeSettle;
        uint64 payout;
        /// @dev When the payout takes effect; 0 = not settled. See `settle`.
        uint64 settleAt;
        /// @dev Who recorded the settlement now pending or in effect.
        address settledBy;
        /// @dev A keeper whose settlement here the owner cancelled: it may not record another.
        address barred;
    }

    mapping(uint256 positionId => Status) internal statuses;

    /// @notice Delay before a new signer takes effect. Revoking (zero) is immediate.
    uint256 public constant ROLE_DELAY = 2 days;
    /// @notice A proposed signer must be accepted within this long after `ROLE_DELAY`,
    ///         so a forgotten proposal cannot be accepted months later.
    uint256 public constant ACCEPT_WINDOW = 7 days;
    /// @notice A settlement takes effect this long after the keeper records it,
    ///         and the owner can cancel it meanwhile: one stolen keeper key cannot
    ///         settle a cheap market at $1 and redeem the float out of it. Long
    ///         enough for an owner whose key is kept offline to be reached.
    uint256 public constant SETTLE_DELAY = 6 hours;

    /// @notice Signs quotes off chain. Holds no funds.
    address public signer;
    address public pendingSigner;
    uint256 public pendingSignerAt;
    /// @notice Pauses and settles markets.
    address public keeper;

    event SignerSet(address indexed signer);
    event SignerProposed(address indexed signer, uint256 effectiveAt);
    event KeeperSet(address indexed keeper);
    event PausedSet(uint256 indexed positionId, bool paused);
    event Settled(uint256 indexed positionId, uint64 payout, uint64 effectiveAt);
    event SettleCancelled(uint256 indexed positionId);

    error OnlyKeeper();
    error BadSignature();
    error QuoteExpired(uint64 validUntil);
    error QuoteTooLong(uint64 validUntil);
    error WrongSide(uint8 side);
    error PriceOutOfRange(uint256 positionId, uint64 price);
    error AlreadySettled(uint256 positionId);
    error MarketPaused(uint256 positionId);
    error MarketSettled(uint256 positionId);
    error BadParams();
    error NotYet(uint256 effectiveAt);
    error NothingPending();

    modifier onlyKeeper() {
        if (msg.sender != keeper) revert OnlyKeeper();
        _;
    }

    constructor(address owner_, address signer_, address keeper_) Ownable(owner_) EIP712("Polypad Oracle", "1") {
        signer = signer_;
        keeper = keeper_;
        emit SignerSet(signer_);
        emit KeeperSet(keeper_);
    }

    /**
     * @notice Change the quote signer. A new signer takes effect `ROLE_DELAY`
     *         after this call (via `acceptSigner`), so a stolen owner key cannot
     *         start signing prices before anyone notices. Zero revokes at once.
     */
    function setSigner(address signer_) external onlyOwner {
        if (signer_ == address(0)) {
            signer = address(0);
            pendingSigner = address(0);
            emit SignerSet(address(0));
            return;
        }
        pendingSigner = signer_;
        pendingSignerAt = block.timestamp + ROLE_DELAY;
        emit SignerProposed(signer_, pendingSignerAt);
    }

    function acceptSigner() external onlyOwner {
        if (pendingSigner == address(0) || block.timestamp > pendingSignerAt + ACCEPT_WINDOW) revert NothingPending();
        if (block.timestamp < pendingSignerAt) revert NotYet(pendingSignerAt);
        signer = pendingSigner;
        pendingSigner = address(0);
        emit SignerSet(signer);
    }

    function setKeeper(address keeper_) external onlyOwner {
        keeper = keeper_;
        emit KeeperSet(keeper_);
    }

    function setPaused(uint256[] calldata ids, bool paused_) external onlyKeeper {
        for (uint256 i; i < ids.length; ++i) {
            Status storage st = statuses[ids[i]];
            // A market with a recorded payout is decided: buying never reopens.
            // Only the owner's cancelSettle lifts that pause.
            if (!paused_ && st.settleAt != 0) continue;
            st.paused = paused_;
            emit PausedSet(ids[i], paused_);
        }
    }

    /**
     * @notice Record the final payout per share, 0..1e6. It takes effect after
     *         SETTLE_DELAY, and buying stops at once. One-way once in effect.
     *         Keeper or owner. A keeper whose settlement on this market the owner
     *         cancelled may not record another there (its key may be stolen); a
     *         new keeper may.
     */
    function settle(uint256 id, uint64 payout) external {
        Status storage s = statuses[id];
        if (msg.sender != owner() && (msg.sender != keeper || msg.sender == s.barred)) revert OnlyKeeper();
        if (payout > ONE) revert PriceOutOfRange(id, payout);
        if (s.settleAt != 0) revert AlreadySettled(id);
        uint64 at = uint64(block.timestamp + SETTLE_DELAY);
        s.pausedBeforeSettle = s.paused;
        s.paused = true;
        s.payout = payout;
        s.settleAt = at;
        s.settledBy = msg.sender;
        emit PausedSet(id, true);
        emit Settled(id, payout, at);
    }

    /// @notice Withdraw a settlement that has not taken effect yet (a wrong or forged payout).
    function cancelSettle(uint256 id) external onlyOwner {
        Status storage s = statuses[id];
        if (s.settleAt == 0 || block.timestamp >= s.settleAt) revert NothingPending();
        s.settleAt = 0;
        s.payout = 0;
        if (s.settledBy != owner()) s.barred = s.settledBy;
        s.settledBy = address(0);
        // A wrong settlement leaves the market as it was before it: paused only if
        // it already was (say, for its end date). The keeper that recorded it can
        // no longer settle this market; the owner, or a new keeper, can.
        bool was = s.pausedBeforeSettle;
        s.pausedBeforeSettle = false;
        if (!was) {
            s.paused = false;
            emit PausedSet(id, false);
        }
        emit SettleCancelled(id);
    }

    /// @notice The recorded payout and when it takes effect (0 = none recorded).
    function settlement(uint256 id) external view returns (uint64 payout, uint64 settleAt) {
        Status memory s = statuses[id];
        return (s.payout, s.settleAt);
    }

    function _settled(Status memory s) internal view returns (bool) {
        return s.settleAt != 0 && block.timestamp >= s.settleAt;
    }

    /**
     * @notice The price in a quote, after checking it was signed by `signer`, is
     *         live, is for `side`, and the market is open for it. A BUY needs the
     *         market unpaused; a SELL works on a paused market. Neither works on a
     *         settled market: redemptions there use `status` instead.
     */
    function verify(Quote calldata q, bytes calldata sig, uint8 side) external view returns (uint64 price) {
        (price,) = _verify(q, sig, side);
    }

    /// @notice `verify`, also returning the quote's EIP-712 digest (the exchange tracks fills by it).
    function verifyWithDigest(Quote calldata q, bytes calldata sig, uint8 side)
        external
        view
        returns (uint64 price, bytes32 digest)
    {
        return _verify(q, sig, side);
    }

    function _verify(Quote calldata q, bytes calldata sig, uint8 side) internal view returns (uint64, bytes32 digest) {
        if (q.side != side) revert WrongSide(q.side);
        if (block.timestamp > q.validUntil) revert QuoteExpired(q.validUntil);
        if (q.validUntil > block.timestamp + MAX_VALIDITY) revert QuoteTooLong(q.validUntil);
        if (q.price == 0 || q.price > MAX_PRICE) revert PriceOutOfRange(q.positionId, q.price);
        digest = quoteDigest(q);
        if (ECDSA.recover(digest, sig) != signer) revert BadSignature();

        Status memory s = statuses[q.positionId];
        if (_settled(s)) revert MarketSettled(q.positionId);
        if (side == BUY && s.paused) revert MarketPaused(q.positionId);
        return (q.price, digest);
    }

    function quoteDigest(Quote calldata q) public view returns (bytes32) {
        return _hashTypedDataV4(
            keccak256(abi.encode(QUOTE_TYPEHASH, q.positionId, q.side, q.price, q.maxAmount, q.validUntil))
        );
    }

    function status(uint256 id) external view returns (bool paused, bool settled, uint64 payout) {
        Status memory s = statuses[id];
        bool done = _settled(s);
        return (s.paused, done, done ? s.payout : 0);
    }

    /// @dev Disabled: a contract without an owner could never be resumed or reconfigured.
    function renounceOwnership() public pure override {
        revert BadParams();
    }
}
