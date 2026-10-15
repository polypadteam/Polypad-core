// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/**
 * @title PriceFeed
 * @notice The live Polymarket price of each market with a Polypad coin, posted
 *         on chain so anyone can check a pToken's peg: the midpoint, best bid
 *         and best ask of the outcome's order book, six decimals ($1 = 1e6),
 *         with the time they were posted.
 *
 * Display and verification only. Nothing in Polypad trades at these prices:
 * every mint and sale carries a quote signed from the book at that moment
 * (`PriceOracle`), so a stale or wrong post here can mislead a reader but never
 * move money. The keeper posts when a price moves, and at least every few
 * minutes while a market is open; `updatedAt` says how fresh a price is.
 */
contract PriceFeed is Ownable2Step {
    uint64 public constant ONE = 1e6;

    struct Price {
        uint64 mid;
        uint64 bid;
        uint64 ask;
        uint64 updatedAt;
    }

    /// @notice The latest posted price of each market (Polymarket position id).
    mapping(uint256 positionId => Price) public prices;
    /// @notice The address allowed to post (Polypad's keeper); the owner may too.
    address public poster;

    event PricePosted(uint256 indexed positionId, uint64 mid, uint64 bid, uint64 ask);
    event PosterSet(address indexed poster);

    error NotPoster();
    error BadPrice(uint256 positionId);
    error LengthMismatch();

    constructor(address owner_, address poster_) Ownable(owner_) {
        poster = poster_;
        emit PosterSet(poster_);
    }

    function setPoster(address poster_) external onlyOwner {
        poster = poster_;
        emit PosterSet(poster_);
    }

    /**
     * @notice Post prices for several markets at once. Each needs
     *         bid <= mid <= ask <= $1; a bid or ask of 0 means that side of the
     *         book is empty.
     */
    function post(uint256[] calldata ids, uint64[] calldata mids, uint64[] calldata bids, uint64[] calldata asks)
        external
    {
        if (msg.sender != poster && msg.sender != owner()) revert NotPoster();
        uint256 n = ids.length;
        if (mids.length != n || bids.length != n || asks.length != n) revert LengthMismatch();
        for (uint256 i; i < n; ++i) {
            uint64 mid = mids[i];
            uint64 bid = bids[i];
            uint64 ask = asks[i];
            if (mid > ONE || bid > mid || (ask != 0 && ask < mid) || ask > ONE) revert BadPrice(ids[i]);
            prices[ids[i]] = Price(mid, bid, ask, uint64(block.timestamp));
            emit PricePosted(ids[i], mid, bid, ask);
        }
    }

    /// @notice The latest midpoint of a market and when it was posted (0, 0 if never).
    function latest(uint256 positionId) external view returns (uint64 mid, uint64 updatedAt) {
        Price memory p = prices[positionId];
        return (p.mid, p.updatedAt);
    }
}
