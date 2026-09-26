// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";

/**
 * @title PriceOracle
 * @notice Polymarket prices, two ways, plus each market's pause and settlement.
 *
 * **Signed quotes** (the main path). The pricer reads the live Polymarket book
 * and signs a `Quote` when a user asks to trade; the quote rides inside the
 * user's own transaction and the exchange checks it here. A quote is only
 * seconds old when it is used and is sized to the trade.
 *
 * **Posted prices** (so any contract or terminal can trade without asking us).
 * The poster writes a market's midpoint here whenever it moves (about 1%), and
 * calls `alive` every few seconds. So while the poster is alive, every posted
 * price is within that deviation of the real one however long ago it was
 * written, and nobody pays gas to repost a quiet market. A posted price is
 * usable only while the poster has been heard from within `maxPosterSilence`
 * seconds (a dead poster closes the path everywhere at once) and the price is
 * under `maxPostAge` old. A post that jumps by `postJumpAbs` or more halts the
 * posted path for that market for `postCooldown` seconds, so a trader cannot
 * race the lag between a big move and the next post. The exchange adds a wider
 * spread and size caps on top.
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
 */
contract PriceOracle is Ownable2Step, EIP712 {
    uint64 public constant ONE = 1e6;
    /// @notice Longest a quote may be valid for, whatever the signer wrote.
    uint64 public constant MAX_VALIDITY = 30;

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
        bool settled;
        uint64 payout;
    }

    mapping(uint256 positionId => Status) internal statuses;

    struct Posted {
        uint64 price;
        uint64 at;
        uint64 haltedUntil;
    }

    mapping(uint256 positionId => Posted) public posted;

    /// @notice Writes posted prices. Holds no funds.
    address public poster;
    /// @notice Seconds a posted price stays usable while the poster is alive.
    uint32 public maxPostAge = 7_200;
    /// @notice Seconds of poster silence after which no posted price is usable.
    uint32 public maxPosterSilence = 90;
    /// @notice Last time the poster posted or called `alive`.
    uint64 public posterAliveAt;
    /// @notice A post moving the price by at least this much (6 decimals) halts the posted path.
    uint64 public postJumpAbs = 30_000;
    /// @notice Seconds the posted path stays halted after a jump.
    uint32 public postCooldown = 15;

    /// @notice Delay before a new signer or poster takes effect. Revoking (zero) is immediate.
    uint256 public constant ROLE_DELAY = 2 days;

    /// @notice Signs quotes off chain. Holds no funds.
    address public signer;
    address public pendingSigner;
    uint256 public pendingSignerAt;
    address public pendingPoster;
    uint256 public pendingPosterAt;
    /// @notice Pauses and settles markets.
    address public keeper;

    event SignerSet(address indexed signer);
    event SignerProposed(address indexed signer, uint256 effectiveAt);
    event PosterProposed(address indexed poster, uint256 effectiveAt);
    event KeeperSet(address indexed keeper);
    event PausedSet(uint256 indexed positionId, bool paused);
    event Settled(uint256 indexed positionId, uint64 payout);
    event PosterSet(address indexed poster);
    event PostParamsSet(uint32 maxPostAge, uint32 maxPosterSilence, uint64 postJumpAbs, uint32 postCooldown);
    event PricePosted(uint256 indexed positionId, uint64 price, bool halted);

    error OnlyKeeper();
    error BadSignature();
    error QuoteExpired(uint64 validUntil);
    error QuoteTooLong(uint64 validUntil);
    error WrongSide(uint8 side);
    error PriceOutOfRange(uint256 positionId, uint64 price);
    error AlreadySettled(uint256 positionId);
    error MarketPaused(uint256 positionId);
    error MarketSettled(uint256 positionId);
    error OnlyPoster();
    error NoPostedPrice(uint256 positionId);
    error PostStale(uint256 positionId, uint64 at);
    error PosterSilent(uint64 aliveAt);
    error PostHalted(uint256 positionId, uint64 until);
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
        if (pendingSigner == address(0)) revert NothingPending();
        if (block.timestamp < pendingSignerAt) revert NotYet(pendingSignerAt);
        signer = pendingSigner;
        pendingSigner = address(0);
        emit SignerSet(signer);
    }

    function setKeeper(address keeper_) external onlyOwner {
        keeper = keeper_;
        emit KeeperSet(keeper_);
    }

    /// @notice Change the poster: the first set and a revoke (zero) are immediate, a change waits `ROLE_DELAY`.
    function setPoster(address poster_) external onlyOwner {
        if (poster_ == address(0) || poster == address(0)) {
            poster = poster_;
            pendingPoster = address(0);
            emit PosterSet(poster_);
            return;
        }
        pendingPoster = poster_;
        pendingPosterAt = block.timestamp + ROLE_DELAY;
        emit PosterProposed(poster_, pendingPosterAt);
    }

    function acceptPoster() external onlyOwner {
        if (pendingPoster == address(0)) revert NothingPending();
        if (block.timestamp < pendingPosterAt) revert NotYet(pendingPosterAt);
        poster = pendingPoster;
        pendingPoster = address(0);
        emit PosterSet(poster);
    }

    function setPostParams(uint32 maxPostAge_, uint32 maxPosterSilence_, uint64 postJumpAbs_, uint32 postCooldown_)
        external
        onlyOwner
    {
        if (
            maxPostAge_ == 0 || maxPostAge_ > 86_400 || maxPosterSilence_ == 0 || maxPosterSilence_ > 600
                || postJumpAbs_ == 0 || postCooldown_ > 3_600
        ) revert BadParams();
        maxPostAge = maxPostAge_;
        maxPosterSilence = maxPosterSilence_;
        postJumpAbs = postJumpAbs_;
        postCooldown = postCooldown_;
        emit PostParamsSet(maxPostAge_, maxPosterSilence_, postJumpAbs_, postCooldown_);
    }

    /// @notice The poster is running and every posted price is current within its deviation.
    function alive() external {
        if (msg.sender != poster) revert OnlyPoster();
        posterAliveAt = uint64(block.timestamp);
    }

    /// @notice Post midpoints, 6 decimals, strictly inside (0, 1). Also counts as `alive`.
    function post(uint256[] calldata ids, uint64[] calldata prices) external {
        if (msg.sender != poster) revert OnlyPoster();
        if (ids.length != prices.length) revert BadParams();
        posterAliveAt = uint64(block.timestamp);
        for (uint256 i; i < ids.length; ++i) {
            uint64 price = prices[i];
            if (price == 0 || price >= ONE) revert PriceOutOfRange(ids[i], price);
            Posted storage p = posted[ids[i]];
            bool jumped = p.at != 0 && (price > p.price ? price - p.price : p.price - price) >= postJumpAbs;
            if (jumped) p.haltedUntil = uint64(block.timestamp) + postCooldown;
            p.price = price;
            p.at = uint64(block.timestamp);
            emit PricePosted(ids[i], price, jumped);
        }
    }

    /**
     * @notice The posted midpoint for a trade on `side`, after checking it is
     *         fresh, not halted by a recent jump, and the market is open for it
     *         (same rules as `verify`). The exchange applies its own spread.
     */
    function postedPrice(uint256 id, uint8 side) external view returns (uint64) {
        if (side != BUY && side != SELL) revert WrongSide(side);
        Posted memory p = posted[id];
        if (p.at == 0) revert NoPostedPrice(id);
        if (block.timestamp > posterAliveAt + maxPosterSilence) revert PosterSilent(posterAliveAt);
        if (block.timestamp > p.at + maxPostAge) revert PostStale(id, p.at);
        if (block.timestamp < p.haltedUntil) revert PostHalted(id, p.haltedUntil);
        Status memory s = statuses[id];
        if (s.settled) revert MarketSettled(id);
        if (side == BUY && s.paused) revert MarketPaused(id);
        return p.price;
    }

    function setPaused(uint256[] calldata ids, bool paused_) external onlyKeeper {
        for (uint256 i; i < ids.length; ++i) {
            statuses[ids[i]].paused = paused_;
            emit PausedSet(ids[i], paused_);
        }
    }

    /// @notice Record the final payout per share, 0..1e6. One-way.
    function settle(uint256 id, uint64 payout) external onlyKeeper {
        if (payout > ONE) revert PriceOutOfRange(id, payout);
        Status storage s = statuses[id];
        if (s.settled) revert AlreadySettled(id);
        s.settled = true;
        s.payout = payout;
        emit Settled(id, payout);
    }

    /**
     * @notice The price in a quote, after checking it was signed by `signer`, is
     *         live, is for `side`, and the market is open for it. A BUY needs the
     *         market unpaused; a SELL works on a paused market. Neither works on a
     *         settled market: redemptions there use `status` instead.
     */
    function verify(Quote calldata q, bytes calldata sig, uint8 side) external view returns (uint64) {
        if (q.side != side) revert WrongSide(q.side);
        if (block.timestamp > q.validUntil) revert QuoteExpired(q.validUntil);
        if (q.validUntil > block.timestamp + MAX_VALIDITY) revert QuoteTooLong(q.validUntil);
        if (q.price == 0 || q.price >= ONE) revert PriceOutOfRange(q.positionId, q.price);
        if (ECDSA.recover(quoteDigest(q), sig) != signer) revert BadSignature();

        Status memory s = statuses[q.positionId];
        if (s.settled) revert MarketSettled(q.positionId);
        if (side == BUY && s.paused) revert MarketPaused(q.positionId);
        return q.price;
    }

    function quoteDigest(Quote calldata q) public view returns (bytes32) {
        return _hashTypedDataV4(
            keccak256(abi.encode(QUOTE_TYPEHASH, q.positionId, q.side, q.price, q.maxAmount, q.validUntil))
        );
    }

    function status(uint256 id) external view returns (bool paused, bool settled, uint64 payout) {
        Status memory s = statuses[id];
        return (s.paused, s.settled, s.payout);
    }
}
