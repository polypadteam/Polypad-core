// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console} from "forge-std/console.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PToken} from "../../src/PToken.sol";
import {PolypadBase} from "../Polypad.t.sol";
import {ExchangeHandler} from "./ExchangeHandler.sol";

/**
 * Stateful fuzzing of the exchange's money: whatever sequence of trades, posts,
 * pauses, settlements, freezes, desk transfers and queue payments happens, the
 * exchange's USDG, its queue and its share supply must add up exactly.
 */
contract ExchangeInvariantsTest is PolypadBase {
    ExchangeHandler internal h;
    address internal poster = makeAddr("poster");
    uint256 internal initialFloat;
    address[] internal actors;

    function setUp() public override {
        super.setUp();
        vm.startPrank(owner);
        oracle.setPoster(poster);
        exchange.ensurePToken(ID);
        exchange.ensurePToken(ID_B);
        exchange.setOutflowCap(200_000e6);
        vm.stopPrank();

        uint256[] memory ids = new uint256[](2);
        ids[0] = ID;
        ids[1] = ID_B;
        uint64[] memory prices = new uint64[](2);
        prices[0] = 600_000;
        prices[1] = 300_000;
        vm.prank(poster);
        oracle.post(ids, prices);
        uint256[] memory amounts = new uint256[](2);
        amounts[0] = 20_000e6;
        amounts[1] = 20_000e6;
        vm.prank(keeper);
        exchange.reportBacked(ids, amounts);

        for (uint256 i; i < 5; ++i) {
            address a = makeAddr(string(abi.encodePacked("actor", vm.toString(i))));
            actors.push(a);
            usdg.mint(a, 10_000_000e6);
            vm.prank(a);
            usdg.approve(address(exchange), type(uint256).max);
        }
        initialFloat = usdg.balanceOf(address(exchange));

        h = new ExchangeHandler(exchange, oracle, usdg, signerPk, keeper, poster, bridge, [ID, ID_B], actors);
        targetContract(address(h));
        excludeSender(address(exchange));
        excludeSender(address(oracle));
        excludeSender(address(usdg));
    }

    function _unclaimedSum() internal view returns (uint256 sum) {
        for (uint256 i; i < actors.length; ++i) {
            sum += exchange.unclaimed(actors[i]) + exchange.unclaimed(h.sinkOf(actors[i]));
        }
    }

    /// Every USDG the exchange holds is explained by the ghost books, to the unit.
    function invariant_usdgBalanceIsExactlyAccountedFor() public view {
        assertEq(
            usdg.balanceOf(address(exchange)),
            initialFloat + h.ghostUsdgIn() + h.ghostRefilled() - h.ghostReceived() - h.ghostBridged()
        );
    }

    /// `queued` is exactly what was promised to sellers and has not reached them.
    function invariant_queuedIsOwedMinusDelivered() public view {
        assertLe(h.ghostReceived(), h.ghostOwed(), "a seller received more than promised");
        assertEq(exchange.queued(), h.ghostOwed() - h.ghostReceived());
    }

    /// `queued` is the live claims plus the set-aside ones; paid claims are zeroed
    /// (so none can be paid twice) and no live claim is empty.
    function invariant_queueStructure() public view {
        uint256 head = exchange.claimHead();
        uint256 end = head + exchange.queueLength(); // reverts if head > length
        uint256 live;
        for (uint256 i; i < end; ++i) {
            (address to, uint96 amount) = exchange.claims(i);
            if (i < head) {
                assertEq(amount, 0, "a processed claim still holds an amount");
                assertEq(to, address(0));
            } else {
                assertGt(amount, 0, "an empty claim is in line");
                assertTrue(to != address(0), "a claim to the zero address");
                live += amount;
            }
        }
        assertEq(exchange.queued(), live + _unclaimedSum());
    }

    function invariant_freeFloatIsBalanceLessQueued() public view {
        uint256 bal = usdg.balanceOf(address(exchange));
        uint256 q = exchange.queued();
        assertEq(exchange.freeFloat(), bal > q ? bal - q : 0);
    }

    /// Shares exist only as minted minus burned, all held by the actors.
    function invariant_shareSupplyMatchesMintsAndBurns() public view {
        for (uint256 m; m < 2; ++m) {
            PToken p = PToken(address(h.tokens(m)));
            assertEq(p.totalSupply(), h.ghostMinted(m) - h.ghostBurned(m));
            uint256 held;
            for (uint256 i; i < actors.length; ++i) {
                held += p.balanceOf(actors[i]);
            }
            assertEq(held, p.totalSupply());
        }
    }

    /// The hour's outflow never exceeds the cap.
    function invariant_outflowWithinCap() public view {
        assertLe(exchange.outflowInHour(block.timestamp / 3_600), exchange.outflowCapPerHour());
    }

    /// Coverage probe (run with -vv and FOUNDRY_INVARIANT_RUNS=1 to see what the fuzzer reaches).
    function afterInvariant() external view {
        if (vm.envOr("EX_HARNESS_COVERAGE", false)) {
            string[10] memory k = [
                "mint", "redeem", "mintPosted", "redeemPosted", "payQueue", "freeze", "withdrawUnclaimed", "settle", "post", "queueThenFreeze"
            ];
            for (uint256 i; i < k.length; ++i) {
                console.log(k[i], h.hits(keccak256(bytes(k[i]))));
            }
            console.log("queued", exchange.queued(), "claims", exchange.claimHead() + exchange.queueLength());
        }
    }
}
