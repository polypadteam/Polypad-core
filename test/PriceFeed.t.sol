// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

import {PriceFeed} from "../src/PriceFeed.sol";

contract PriceFeedTest is Test {
    PriceFeed internal feed;
    address internal owner = makeAddr("owner");
    address internal keeper = makeAddr("keeper");
    address internal eve = makeAddr("eve");

    function setUp() public {
        feed = new PriceFeed(owner, keeper);
    }

    function _one(uint256 id, uint64 mid, uint64 bid, uint64 ask)
        internal
        pure
        returns (uint256[] memory ids, uint64[] memory mids, uint64[] memory bids, uint64[] memory asks)
    {
        ids = new uint256[](1);
        mids = new uint64[](1);
        bids = new uint64[](1);
        asks = new uint64[](1);
        (ids[0], mids[0], bids[0], asks[0]) = (id, mid, bid, ask);
    }

    function test_keeperPostsAndAnyoneReads() public {
        (uint256[] memory ids, uint64[] memory mids, uint64[] memory bids, uint64[] memory asks) =
            _one(7, 670_000, 660_000, 680_000);
        vm.warp(1_800_000_000);
        vm.expectEmit(address(feed));
        emit PriceFeed.PricePosted(7, 670_000, 660_000, 680_000);
        vm.prank(keeper);
        feed.post(ids, mids, bids, asks);
        (uint64 mid, uint64 at) = feed.latest(7);
        assertEq(mid, 670_000);
        assertEq(at, 1_800_000_000);
        (uint64 m, uint64 b, uint64 a, uint64 u) = feed.prices(7);
        assertEq(m, 670_000);
        assertEq(b, 660_000);
        assertEq(a, 680_000);
        assertEq(u, 1_800_000_000);
    }

    function test_onlyPosterOrOwnerPosts() public {
        (uint256[] memory ids, uint64[] memory mids, uint64[] memory bids, uint64[] memory asks) =
            _one(7, 500_000, 490_000, 510_000);
        vm.prank(eve);
        vm.expectRevert(PriceFeed.NotPoster.selector);
        feed.post(ids, mids, bids, asks);
        vm.prank(owner);
        feed.post(ids, mids, bids, asks);
    }

    function test_rejectsImpossiblePrices() public {
        uint64[4][4] memory bad = [
            [uint64(1_000_001), 0, 0, uint64(7)], // mid over $1
            [uint64(500_000), 600_000, 700_000, 7], // bid over mid
            [uint64(500_000), 400_000, 450_000, 7], // ask under mid
            [uint64(500_000), 400_000, 1_000_001, 7] // ask over $1
        ];
        for (uint256 i; i < bad.length; ++i) {
            (uint256[] memory ids, uint64[] memory mids, uint64[] memory bids, uint64[] memory asks) =
                _one(bad[i][3], bad[i][0], bad[i][1], bad[i][2]);
            vm.prank(keeper);
            vm.expectRevert(abi.encodeWithSelector(PriceFeed.BadPrice.selector, uint256(7)));
            feed.post(ids, mids, bids, asks);
        }
        // An empty side of the book is posted as 0.
        (uint256[] memory i2, uint64[] memory m2, uint64[] memory b2, uint64[] memory a2) = _one(7, 990_000, 0, 0);
        vm.prank(keeper);
        feed.post(i2, m2, b2, a2);
    }

    function test_lengthsMustMatch() public {
        uint256[] memory ids = new uint256[](2);
        uint64[] memory one = new uint64[](1);
        vm.prank(keeper);
        vm.expectRevert(PriceFeed.LengthMismatch.selector);
        feed.post(ids, one, one, one);
    }

    function test_setPosterIsOwnerOnly() public {
        vm.prank(eve);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, eve));
        feed.setPoster(eve);
        vm.prank(owner);
        feed.setPoster(eve);
        assertEq(feed.poster(), eve);
    }
}
