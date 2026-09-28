// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console} from "forge-std/console.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PToken} from "../../src/PToken.sol";
import {PolypadBase} from "../Polypad.t.sol";
import {ExchangeHandler} from "./ExchangeHandler.sol";

/**
 * Stateful fuzzing of the exchange's money: whatever sequence of trades, price
 * moves, delayed sales, pauses, settlements, freezes, desk transfers and queue payments happens, the
 * exchange's USDG, its queue and its share supply must add up exactly.
 */
contract ExchangeInvariantsTest is PolypadBase {
    ExchangeHandler internal h;
    uint256 internal initialFloat;
    address[] internal actors;

    function setUp() public override {
        super.setUp();
        vm.startPrank(owner);
        exchange.ensurePToken(ID);
        exchange.ensurePToken(ID_B);
        // Low enough that runs of sales go over it and get delayed: 3% of the
        // float (about $3,000 to start, less once the desk takes it), at least $2,000.
        exchange.setOutflowCap(2_000e6, 300);
        // Room to mint whatever the keeper last reported, so trades keep landing.
        exchange.setMaxRisk(ID, 50_000e6);
        exchange.setMaxRisk(ID_B, 50_000e6);
        vm.stopPrank();

        uint256[] memory ids = new uint256[](2);
        ids[0] = ID;
        ids[1] = ID_B;
        uint64[] memory prices = new uint64[](2);
        prices[0] = 600_000;
        prices[1] = 300_000;
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

        h = new ExchangeHandler(exchange, oracle, usdg, signerPk, keeper, bridge, [ID, ID_B], actors);
        targetContract(address(h));
        excludeSender(address(exchange));
        excludeSender(address(oracle));
        excludeSender(address(usdg));
    }

    /// The handler's delayed-sale paths do what the invariants assume: a sale over
    /// the cap is split, the delayed part is released after DELAY or cancelled.
    function test_handlerDelaysReleasesAndCancels() public {
        // Distinct prices, so each trade signs a distinct quote.
        for (uint256 i; i < 3; ++i) {
            h.mint(i, 0, 3_000e6, 40_000 + i);
        }
        h.redeem(0, 0, 100, 40_000); // ~$3,000 against a ~$3,090 cap
        h.redeem(1, 0, 100, 40_001); // over it: mostly delayed
        h.redeem(2, 0, 100, 40_002); // all delayed
        assertGe(h.knownDelayed(), 2, "nothing was delayed");
        assertEq(
            uint64(uint256(vm.load(address(exchange), bytes32(HOUR_FLOAT_SLOT)))),
            block.timestamp / 3_600,
            "the hour's float snapshot was not found"
        );
        invariant_hourOutflowWithinSnapshotCap();
        assertGt(h.ghostDelayed(), 0);
        invariant_delayedSalesAddUp();
        invariant_outflowWithinCap();

        h.release(0); // too early: nothing happens
        h.cancel(1);
        vm.warp(block.timestamp + exchange.DELAY());
        h.release(0);
        assertEq(h.ghostDelayed(), 0, "a delayed sale is still pending");
        invariant_delayedSalesAddUp();
        invariant_shareSupplyMatchesMintsAndBurns();
        invariant_usdgBalanceIsExactlyAccountedFor();
        invariant_queuedIsOwedMinusDelivered();
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

    /// Shares exist only as minted minus burned, held by the actors (or their sinks,
    /// where a cancelled sale returns them) and by the exchange for delayed sales.
    function invariant_shareSupplyMatchesMintsAndBurns() public view {
        for (uint256 m; m < 2; ++m) {
            PToken p = PToken(address(h.tokens(m)));
            assertEq(p.totalSupply(), h.ghostMinted(m) - h.ghostBurned(m));
            uint256 held = p.balanceOf(address(exchange));
            for (uint256 i; i < actors.length; ++i) {
                held += p.balanceOf(actors[i]) + p.balanceOf(h.sinkOf(actors[i]));
            }
            assertEq(held, p.totalSupply());
        }
    }

    /// Live delayed sales hold exactly the pToken the exchange keeps for them, and
    /// owe exactly what the ghost books say is held back.
    function invariant_delayedSalesAddUp() public view {
        uint256 owed;
        uint256[2] memory shares;
        uint256 n = h.knownDelayed();
        for (uint256 i; i < n; ++i) {
            (address to,, uint256 id, uint128 pAmount, uint128 amount) = exchange.delayed(i);
            if (to == address(0)) {
                assertEq(amount, 0, "a finished delayed sale still owes");
                continue;
            }
            owed += amount;
            shares[id == ID ? 0 : 1] += pAmount;
        }
        assertEq(owed, h.ghostDelayed());
        for (uint256 m; m < 2; ++m) {
            uint256 id = m == 0 ? ID : ID_B;
            assertEq(exchange.delayedShares(id), shares[m]);
            assertEq(PToken(address(h.tokens(m))).balanceOf(address(exchange)), shares[m]);
        }
    }

    /// @dev PExchange's `hourFloat` slot (`forge inspect PExchange storageLayout`).
    /// `hourFloat` (uint64 hour | uint96 amount | uint96 base), from `forge inspect PExchange storage-layout`.
    uint256 internal constant HOUR_FLOAT_SLOT = 26;

    /// Within a clock hour, what left through redemptions (paid or queued) never
    /// passes the cap fixed by the hour's float snapshot. Mints add no room.
    function invariant_hourOutflowWithinSnapshotCap() public view {
        uint256 word = uint256(vm.load(address(exchange), bytes32(HOUR_FLOAT_SLOT)));
        uint256 snapHour = uint64(word);
        uint256 base = uint96(word >> 160);
        uint256 hour = block.timestamp / 3_600;
        assertLe(snapHour, hour, "hourFloat slot moved: update HOUR_FLOAT_SLOT");
        if (snapHour != hour) return;
        uint256 cap = (base * exchange.outflowFloatBps()) / 10_000;
        if (cap < exchange.outflowFloor()) cap = exchange.outflowFloor();
        // Exactly the exchange's rule: remaining = cap - outflow over the sliding
        // hour (floored at 0); mints add no room.
        uint256 into = block.timestamp % 3_600;
        uint256 out = exchange.outflowInHour(hour) + (exchange.outflowInHour(hour - 1) * (3_600 - into)) / 3_600;
        assertEq(exchange.outflowRemaining(), out >= cap ? 0 : cap - out, "remaining != cap - outflow");
        assertLe(exchange.outflowInHour(hour), cap);
    }

    /// Backing never exceeds supply: a report is clamped to supply, and every
    /// burn (sale, release, absorb) lowers backing by what it burned. So the gap
    /// between a burn and the desk's next report can never be minted unhedged.
    function invariant_backedNeverAboveSupply() public view {
        for (uint256 m; m < 2; ++m) {
            uint256 id = m == 0 ? ID : ID_B;
            assertLe(exchange.backed(id), exchange.pTokenOf(id).totalSupply(), "backing above supply");
        }
    }

    /// No redemption paid or queued more at once than the hour's cap had left,
    /// and `absorb` never burned shares held for a delayed sale.
    function invariant_outflowWithinCap() public view {
        assertFalse(h.ghostOverCap(), "a sale went past the cap undelayed");
        assertFalse(h.ghostAbsorbedDelayed(), "absorb burned delayed shares");
    }

    /// Coverage probe (run with -vv and FOUNDRY_INVARIANT_RUNS=1 to see what the fuzzer reaches).
    function afterInvariant() external view {
        if (vm.envOr("EX_HARNESS_COVERAGE", false)) {
            string[11] memory k = [
                "mint",
                "redeem",
                "delayed",
                "release",
                "cancel",
                "payQueue",
                "freeze",
                "withdrawUnclaimed",
                "settle",
                "post",
                "queueThenFreeze"
            ];
            for (uint256 i; i < k.length; ++i) {
                console.log(k[i], h.hits(keccak256(bytes(k[i]))));
            }
            console.log("queued", exchange.queued(), "claims", exchange.claimHead() + exchange.queueLength());
        }
    }
}
