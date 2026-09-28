// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {FeeVault} from "../../src/FeeVault.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";

import {PolypadBase} from "../Polypad.t.sol";

/// @dev Calls the vault's transfer hook as if it were a coin.
contract FakeCoin {
    function poke(FeeVault v, address from, address to) external {
        v.onTransfer(from, to);
    }

    function balanceOf(address) external pure returns (uint256) {
        return 1e27;
    }
}

/**
 * @dev Targeted attacks and edge cases on the FeeVault and the coin transfer
 *      hook: sniping the stream, same-block races, duplicate and hostile claim
 *      batches, rounding at both extremes, the hook never freezing a coin,
 *      stream-reset griefing, and who may do what.
 */
contract FeeVaultAttacks is PolypadBase {
    address internal carol = makeAddr("carol");
    address internal sniper = makeAddr("sniper");

    function setUp() public override {
        super.setUp();
        address[2] memory more = [carol, sniper];
        for (uint256 i; i < 2; ++i) {
            usdg.mint(more[i], 1_000_000e6);
            vm.prank(more[i]);
            usdg.approve(address(router), type(uint256).max);
        }
        usdg.mint(alice, 1_000_000e6);
        usdg.mint(bob, 1_000_000e6);
        usdg.mint(address(exchange), 10_000_000e6);
        vm.prank(owner);
        exchange.setMaxRisk(ID, type(uint256).max);
    }

    function _p(Coin c) internal view returns (PToken) {
        (,, address p,,) = vault.launches(address(c));
        return PToken(p);
    }

    function _book(Coin c) internal view returns (uint256 acc, uint256 supply, uint256 pot, uint256 streaming) {
        (acc, supply, pot, streaming,,) = vault.books(address(c));
    }

    /// Everything the vault holds in `c`'s pToken is creator fees, the pot or the stream.
    function _conserved(Coin c) internal view {
        (,, uint256 pot, uint256 streaming) = _book(c);
        PToken p = _p(c);
        assertEq(p.balanceOf(address(vault)), pot + streaming + vault.owed(address(p), creator), "pToken books");
    }

    /// Mint `amount` USDG worth of pToken to this test and donate it to `c`'s holders.
    function _donate(Coin c, uint256 shares) internal {
        PToken p = _p(c);
        if (p.balanceOf(address(this)) < shares) {
            usdg.mint(address(this), 1_000_000e6);
            usdg.approve(address(exchange), type(uint256).max);
            (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
            exchange.mint(address(p), 500_000e6, 0, address(this), q, sig);
        }
        p.approve(address(vault), shares);
        vault.deposit(address(c), address(p), shares);
    }

    function _none() internal view returns (PriceOracle.Quote memory q, bytes memory sig) {
        return signedQuote(ID, SELL);
    }

    /* ------------------------------------------------------------ sniping */

    /// A sniper who buys right after a big fee lands and leaves `dt` later takes
    /// at most their share of what streamed in `dt`, never the lump.
    function testFuzz_sniperTakesOnlyTheTimeTheyHeld(uint256 dt, uint256 size) public {
        dt = bound(dt, 0, 2 hours);
        size = bound(size, 100e6, 5_000e6);
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 3_000e6);
        vm.warp(block.timestamp + 2 hours);
        // A big lump for holders (as a pool collect would deliver).
        _donate(c, 50_000e6);
        (,,, uint256 streamingBefore) = _book(c);

        (uint256 got,) = _buy(sniper, curve, size);
        (,,, uint256 streaming) = _book(c);
        uint256 share = (c.balanceOf(sniper) * 1e18) / (c.balanceOf(sniper) + c.balanceOf(alice) + c.balanceOf(platform));
        vm.warp(block.timestamp + dt);
        uint256 taken = vault.pending(address(c), sniper);
        _sell(sniper, curve, got);

        // Released in dt is at most streaming x dt / STREAM (every deposit restarts the hour).
        uint256 cap = dt >= 1 hours ? streaming : (streaming * dt) / 1 hours;
        assertLe(taken, (cap * share) / 1e18 + 2, "sniper beat the stream");
        assertLe(taken, streamingBefore + streaming, "more than was ever there");
        if (dt == 0) assertEq(taken, 0, "same-block sniper earned");
        _conserved(c);
    }

    /// Buy, hand the bag to a second wallet, sell from there: all in one block, nothing earned.
    function test_flashHoldAcrossWalletsEarnsNothing() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 10 minutes);
        _donate(c, 10_000e6);
        vm.warp(block.timestamp + 1);
        (uint256 got,) = _buy(sniper, curve, 4_000e6);
        vm.prank(sniper);
        c.transfer(carol, got);
        _sell(carol, curve, got);
        assertEq(vault.pending(address(c), sniper), 0);
        assertEq(vault.pending(address(c), carol), 0);
        _conserved(c);
    }

    /* -------------------------------------------------------------- races */

    function test_twoClaimsInOneBlockPayOnce() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 2 hours);
        uint256 owed = vault.pending(address(c), alice);
        assertGt(owed, 0);
        vm.prank(alice);
        assertEq(vault.claim(address(c), alice), owed);
        vm.prank(alice);
        vm.expectRevert(FeeVault.NothingToClaim.selector);
        vault.claim(address(c), alice);
        // A keeper batch in the same block finds nothing either.
        address[] memory hs = new address[](1);
        hs[0] = alice;
        (PriceOracle.Quote memory q, bytes memory sig) = _none();
        assertEq(vault.claimFor(address(c), hs, 0, false, q, sig), 0);
        assertEq(_p(c).balanceOf(alice), owed);
    }

    /// Moving the bag first does not move what was already earned, nor let the receiver claim the past.
    function test_claimAfterGivingTheBagAway() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 2 hours);
        uint256 earned = vault.pending(address(c), alice);
        uint256 bag = c.balanceOf(alice);
        vm.prank(alice);
        c.transfer(bob, bag);
        assertEq(vault.pending(address(c), bob), 0);
        assertEq(vault.pending(address(c), alice), earned);
        vm.prank(alice);
        assertEq(vault.claim(address(c), alice), earned);
        vm.prank(bob);
        vm.expectRevert(FeeVault.NothingToClaim.selector);
        vault.claim(address(c), bob);
    }

    function test_selfAndZeroTransfersChangeNothing() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 30 minutes);
        uint256 before = vault.pending(address(c), alice);
        (, uint256 supply,,) = _book(c);
        uint256 bal = c.balanceOf(alice);
        vm.startPrank(alice);
        c.transfer(alice, bal);
        c.transfer(bob, 0);
        vm.stopPrank();
        // A zero transfer from an address that never held anything, to an excluded address.
        vm.prank(makeAddr("nobody"));
        c.transfer(address(poolManager), 0);
        assertEq(vault.pending(address(c), alice), before);
        assertEq(vault.pending(address(c), bob), 0);
        (, uint256 supplyAfter,,) = _book(c);
        assertEq(supplyAfter, supply);
        _conserved(c);
    }

    function test_duplicateHoldersInABatchArePaidOnce() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        _buy(bob, curve, 1_000e6);
        vm.warp(block.timestamp + 2 hours);
        uint256 pa = vault.pending(address(c), alice);
        uint256 pb = vault.pending(address(c), bob);
        address[] memory hs = new address[](6);
        hs[0] = alice;
        hs[1] = alice;
        hs[2] = bob;
        hs[3] = alice;
        hs[4] = address(poolManager);
        hs[5] = bob;
        (PriceOracle.Quote memory q, bytes memory sig) = _none();
        assertEq(vault.claimFor(address(c), hs, 0, false, q, sig), pa + pb);
        assertEq(_p(c).balanceOf(alice), pa);
        assertEq(_p(c).balanceOf(bob), pb);
        _conserved(c);
    }

    /// A holder cashing out their own rewards twice in one batch gets the USDG once.
    function test_selfCashOutListedTwicePaysOnce() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 2 hours);
        uint256 pa = vault.pending(address(c), alice);
        address[] memory hs = new address[](2);
        hs[0] = alice;
        hs[1] = alice;
        (PriceOracle.Quote memory q, bytes memory sig) = _none();
        vm.prank(alice);
        assertEq(vault.claimFor(address(c), hs, 0, true, q, sig), pa);
        _conserved(c);
    }

    /// Cash-out with a quote for another market, or an expired one: skipped, rewards kept.
    function test_badQuoteOnCashOutIsSkippedNotLost() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 2 hours);
        uint256 pa = vault.pending(address(c), alice);
        address[] memory hs = new address[](1);
        hs[0] = alice;
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID_B, SELL);
        vm.prank(alice);
        assertEq(vault.claimFor(address(c), hs, 0, true, q, sig), 0);
        assertEq(vault.pending(address(c), alice), pa);
        (q, sig) = signedQuote(ID, SELL);
        vm.warp(block.timestamp + 31);
        uint256 pa2 = vault.pending(address(c), alice);
        vm.prank(alice);
        assertEq(vault.claimFor(address(c), hs, 0, true, q, sig), 0);
        assertEq(vault.pending(address(c), alice), pa2);
        _conserved(c);
    }

    /// A long batch costs gas linear in its length; nothing in it can make it revert.
    function test_largeBatchGas() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        uint256 n = 300;
        address[] memory hs = new address[](n);
        (uint256 got,) = _buy(alice, curve, 3_000e6);
        vm.startPrank(alice);
        for (uint256 i; i < n; ++i) {
            hs[i] = address(uint160(0x10000 + i));
            c.transfer(hs[i], got / (n * 2));
        }
        vm.stopPrank();
        vm.warp(block.timestamp + 2 hours);
        (PriceOracle.Quote memory q, bytes memory sig) = _none();
        uint256 g = gasleft();
        uint256 paid = vault.claimFor(address(c), hs, 0, false, q, sig);
        uint256 used = g - gasleft();
        emit log_named_uint("gas per holder", used / n);
        assertGt(paid, 0);
        assertLt(used / n, 90_000);
        _conserved(c);
    }

    /* ----------------------------------------------------------- rounding */

    /// 1-wei and dust deposits against the whole float of coins: books stay exact, promises covered.
    function testFuzz_dustDepositsAgainstHugeSupply(uint256 seed) public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 6_000e6);
        _buy(bob, curve, 900e6);
        for (uint256 i; i < 40; ++i) {
            uint256 r = uint256(keccak256(abi.encode(seed, i)));
            _donate(c, 1 + r % 1_000);
            vm.warp(block.timestamp + r % 10 minutes);
        }
        vm.warp(block.timestamp + 2 hours);
        (,, uint256 pot, uint256 streaming) = _book(c);
        assertLe(vault.pending(address(c), alice) + vault.pending(address(c), bob) + vault.pending(address(c), platform), pot + streaming);
        _conserved(c);
    }

    /// A huge deposit against the smallest supply that streams at all (MIN_SUPPLY).
    function testFuzz_hugeDepositAgainstTinySupply(uint256 keep, uint256 amount) public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        (uint256 got,) = _buy(alice, curve, 100e6);
        keep = bound(keep, vault.MIN_SUPPLY(), 10 * vault.MIN_SUPPLY());
        assertGt(got, keep);
        amount = bound(amount, 1, 400_000e6);
        vm.startPrank(alice);
        c.transfer(curve.DEAD(), got - keep);
        vm.stopPrank();
        // The platform's graduation dust is not in play (no graduation yet).
        _donate(c, amount);
        vm.warp(block.timestamp + 2 hours);
        uint256 pa = vault.pending(address(c), alice);
        (,, uint256 pot, uint256 streaming) = _book(c);
        assertLe(pa, pot + streaming);
        vm.prank(alice);
        vault.claim(address(c), alice);
        _conserved(c);
    }

    /// Below MIN_SUPPLY held by wallets nothing streams: a deposit waits, whole,
    /// and streams to whoever holds once supply is back above it.
    function testFuzz_depositAgainstSupplyBelowMinWaits(uint256 keep, uint256 amount) public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        (uint256 got,) = _buy(alice, curve, 100e6);
        keep = bound(keep, 1, vault.MIN_SUPPLY() - 1);
        amount = bound(amount, 1e6, 400_000e6);
        address dead = curve.DEAD();
        vm.prank(alice);
        c.transfer(dead, got - keep);
        uint256 pa0 = vault.pending(address(c), alice);
        (,,, uint256 streaming0) = _book(c);
        _donate(c, amount);
        vm.warp(block.timestamp + 5 hours);
        assertEq(vault.pending(address(c), alice), pa0, "a dust holder collects nothing");
        vm.prank(alice);
        c.transfer(bob, 0); // pokes the book: still nothing released
        (,,, uint256 streaming) = _book(c);
        assertEq(streaming, streaming0 + amount, "the deposit waits, whole");

        // A real holder arrives: the stream resumes and pays out within the hour.
        _buy(bob, curve, 100e6);
        vm.warp(block.timestamp + 1 hours + 1);
        vm.prank(bob);
        c.transfer(bob, 0); // books update on the next touch
        (,,, streaming) = _book(c);
        assertLe(streaming, 1_000, "released once supply is back");
        assertGt(vault.pending(address(c), bob), 0);
        _conserved(c);
    }

    /// A thousand tiny deposits over time: what is left after everyone is paid is dust.
    function test_thousandSmallDepositsDrift() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 1_000e6);
        _buy(bob, curve, 3_000e6);
        _buy(carol, curve, 333e6);
        for (uint256 i; i < 1_000; ++i) {
            _donate(c, 1_000 + i);
            vm.warp(block.timestamp + 37);
        }
        vm.warp(block.timestamp + 2 hours);
        address[] memory hs = new address[](4);
        hs[0] = alice;
        hs[1] = bob;
        hs[2] = carol;
        hs[3] = platform;
        (PriceOracle.Quote memory q, bytes memory sig) = _none();
        vault.claimFor(address(c), hs, 0, false, q, sig);
        (,, uint256 pot, uint256 streaming) = _book(c);
        assertLe(pot + streaming, 1_000, "dust left behind");
        _conserved(c);
    }

    /* --------------------------------------------------------- hook safety */

    /// Everyone sells out while fees are streaming: transfers keep working, and
    /// the stream waits for the next holder rather than vanishing or reverting.
    function test_supplyToZeroWhileStreamingNeverFreezes() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        (uint256 a,) = _buy(alice, curve, 2_000e6);
        _donate(c, 10_000e6);
        vm.warp(block.timestamp + 5 minutes);
        _sell(alice, curve, a);
        (, uint256 supply,, uint256 streaming) = _book(c);
        assertEq(supply, 0);
        assertGt(streaming, 0);
        uint256 alicePending = vault.pending(address(c), alice);
        vm.warp(block.timestamp + 3 hours);
        // Nobody holds: nothing released, nothing lost.
        assertEq(vault.pending(address(c), alice), alicePending);
        (, supply,, streaming) = _book(c);
        assertGt(streaming, 0);
        // Transfers and trades still work, and the next holder gets the stream.
        _buy(bob, curve, 500e6);
        vm.warp(block.timestamp + 2 hours);
        assertGt(vault.pending(address(c), bob), 9_000e6, "stream reached the new holder");
        _conserved(c);
    }

    /// Everyone sends their coins to the burn address: the coin still transfers.
    function test_allHoldersBurnNeverFreezes() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        (uint256 a,) = _buy(alice, curve, 1_000e6);
        _donate(c, 1_000e6);
        vm.warp(block.timestamp + 10 minutes);
        address dead = curve.DEAD();
        vm.prank(alice);
        c.transfer(dead, a);
        vm.warp(block.timestamp + 10 minutes);
        // Below MIN_SUPPLY: 1 wei of coin left with a wallet.
        (uint256 b,) = _buy(bob, curve, 10e6);
        vm.startPrank(bob);
        c.transfer(dead, b - 1);
        c.transfer(carol, 1);
        vm.stopPrank();
        vm.warp(block.timestamp + 2 hours);
        vm.prank(carol);
        c.transfer(alice, 1);
        assertEq(vault.pending(address(c), carol), 0, "dust holder took the stream");
        _conserved(c);
    }

    /// Something that is not a registered coin calling the hook changes nothing.
    function test_hookIgnoresStrangers() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 1_000e6);
        vm.warp(block.timestamp + 1 hours);
        (uint256 acc, uint256 supply, uint256 pot,) = _book(c);
        uint256 pa = vault.pending(address(c), alice);
        FakeCoin f = new FakeCoin();
        f.poke(vault, alice, bob);
        vault.onTransfer(alice, bob); // from an EOA-like caller (this test)
        (uint256 acc2, uint256 supply2, uint256 pot2,) = _book(c);
        assertEq(acc2, acc);
        assertEq(supply2, supply);
        assertEq(pot2, pot);
        assertEq(vault.pending(address(c), alice), pa);
        assertEq(vault.tracked(address(f), alice), 0);
    }

    /// The hook runs even for the vault's own coin transfers (deposits of coin fees).
    function test_coinFeeDepositDuringHookIsConsistent() public {
        (Coin c, BondingCurve curve) = _launch(ID, 5_000);
        (uint256 a,) = _buy(alice, curve, 1_000e6);
        vm.startPrank(alice);
        c.approve(address(vault), a / 10);
        vault.deposit(address(c), address(c), a / 10);
        vm.stopPrank();
        // Half the coins to the creator, half burned: nobody's tracked balance moves but alice's.
        assertEq(vault.owed(address(c), creator), a / 10 - (a / 10) * 5_000 / 10_000);
        assertEq(vault.tracked(address(c), alice), c.balanceOf(alice));
        assertEq(vault.tracked(address(c), address(vault)), 0);
    }

    /// Regression: the stream is pro rata over wallet holders only. If every other
    /// holder sells into the pool (excluded), a wallet keeping one coin used to
    /// collect the entire holder stream. MIN_SUPPLY is now 0.01% of supply: below
    /// it nothing streams, and the stream waits for real holders.
    function test_loneDustHolderGetsNothingTheStreamWaits() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        (uint256 a,) = _buy(alice, curve, 7_000e6); // graduates; alice holds nearly everything
        (uint256 s,) = _buy(sniper, curve, 1e6);
        vm.prank(sniper);
        c.transfer(carol, s - 1e18); // sniper keeps exactly one coin
        vm.prank(carol);
        c.transfer(curve.DEAD(), 0);
        _sell(alice, curve, a); // alice exits into the pool
        _sell(carol, curve, c.balanceOf(carol));
        // Platform graduation dust is the only other wallet balance.
        vm.warp(block.timestamp + 2 hours);
        uint256 before = vault.pending(address(c), sniper);
        _donate(c, 1_000e6);
        vm.warp(block.timestamp + 2 hours);
        uint256 got = vault.pending(address(c), sniper) - before;
        assertLt(c.balanceOf(sniper) + c.balanceOf(platform), vault.MIN_SUPPLY());
        assertEq(got, 0, "a dust holder collects nothing");
        (,,, uint256 streaming) = _book(c);
        assertGe(streaming, 1_000e6 / 2, "the holders' share waits");

        // A real holder buys in the pool: the stream resumes for them.
        _buy(bob, curve, 100e6);
        assertGe(c.balanceOf(bob), vault.MIN_SUPPLY());
        vm.warp(block.timestamp + 1 hours + 1);
        assertGt(vault.pending(address(c), bob), 1_000e6 / 4);
        assertLe(vault.pending(address(c), sniper) - before, vault.pending(address(c), bob) / 1_000);
        _conserved(c);
    }

    /* ------------------------------------------------------- griefing */

    /// Regression: anyone may deposit, and each deposit used to restart the hour
    /// for everything still streaming, so 1 wei every block held back ~1/e. The
    /// end is now the amount-weighted average of what streams: 1-wei spam barely
    /// moves it, and a real deposit still streams out within its hour.
    function test_streamSpamCannotHoldBackTheStream() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 2 hours);
        uint256 earlier = vault.pending(address(c), alice) + vault.pending(address(c), platform);
        _donate(c, 10_000e6);
        (,,, uint256 start) = _book(c);
        for (uint256 i; i < 1_800; ++i) {
            vm.warp(block.timestamp + 2);
            _donate(c, 1);
        }
        (,,, uint256 left) = _book(c);
        // An hour after the real deposit, despite 1-wei spam every block: >= 99% out.
        assertLe(left, start / 100, "stream held back by spam");
        // Stop griefing: the rest releases shortly after.
        vm.warp(block.timestamp + 1 hours + 1);
        assertApproxEqAbs(vault.pending(address(c), alice) + vault.pending(address(c), platform), earlier + 10_000e6 + 1_800, 1_000);
        _conserved(c);
    }

    /* -------------------------------------------------------- split / modes */

    function testFuzz_splitIsExact(uint16 bps, uint256 amount) public {
        bps = uint16(bound(bps, 0, 10_000));
        amount = bound(amount, 1, 100_000e6);
        (Coin c, BondingCurve curve) = _launch(ID, bps);
        _buy(alice, curve, 100e6);
        PToken p = _p(c);
        uint256 owedBefore = vault.owed(address(p), creator);
        (,,, uint256 sBefore) = _book(c);
        (,, uint256 potBefore,) = _book(c);
        _donate(c, amount);
        (,, uint256 pot, uint256 s) = _book(c);
        uint256 toHolders = (amount * bps) / 10_000;
        assertEq(vault.owed(address(p), creator) - owedBefore, amount - toHolders);
        assertEq((s + pot) - (sBefore + potBefore), toHolders);
    }

    function test_creatorOnlyCoinHasNoHookAndPaysNoHolders() public {
        (Coin c, BondingCurve curve) = _launch(ID, 0);
        assertEq(c.holderBook(), address(0));
        _buy(alice, curve, 1_000e6);
        vm.warp(block.timestamp + 2 hours);
        assertEq(vault.pending(address(c), alice), 0);
        (, uint256 supply,, uint256 streaming) = _book(c);
        assertEq(supply, 0);
        assertEq(streaming, 0);
        vm.prank(alice);
        vm.expectRevert(FeeVault.NothingToClaim.selector);
        vault.claim(address(c), alice);
    }

    /* ------------------------------------------------------ access control */

    function test_depositForUnknownCoinOrWrongAssetReverts() public {
        (Coin c,) = _launch(ID, 10_000);
        (Coin other,) = _launch(ID_B, 10_000);
        address pc = address(_p(c));
        address po = address(_p(other));
        vm.expectRevert(abi.encodeWithSelector(FeeVault.Unknown.selector, address(0xBEEF)));
        vault.deposit(address(0xBEEF), pc, 1);
        // Another market's pToken is not this coin's asset.
        vm.expectRevert(abi.encodeWithSelector(FeeVault.BadAsset.selector, po));
        vault.deposit(address(c), po, 1);
        vm.expectRevert(abi.encodeWithSelector(FeeVault.BadAsset.selector, address(other)));
        vault.deposit(address(c), address(other), 1);
    }

    function test_setPayeeRules() public {
        (Coin c,) = _launch(ID, 5_000);
        vm.prank(alice);
        vm.expectRevert(FeeVault.OnlyPayee.selector);
        vault.setPayee(address(c), alice);
        vm.prank(creator);
        vm.expectRevert(FeeVault.BadParams.selector);
        vault.setPayee(address(c), address(0));
        // Unknown coin: nobody is its payee (the zero address cannot call).
        vm.prank(alice);
        vm.expectRevert(FeeVault.OnlyPayee.selector);
        vault.setPayee(address(0xBEEF), alice);
    }

    function test_withdrawUsdRejectsForeignQuoteAndKeepsTheClaim() public {
        (Coin c, BondingCurve curve) = _launch(ID, 0);
        _buy(alice, curve, 1_000e6);
        PToken p = _p(c);
        uint256 owed = vault.owed(address(p), creator);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID_B, SELL);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteForOtherMarket.selector, ID_B, ID));
        vault.withdrawUsd(address(p), creator, 0, q, sig);
        assertEq(vault.owed(address(p), creator), owed);
        // Coin fees cannot be cashed out as if they were shares.
        vm.prank(creator);
        vm.expectRevert(FeeVault.NothingToClaim.selector);
        vault.withdrawUsd(address(c), creator, 0, q, sig);
        // Nobody else can take the creator's fees.
        vm.prank(alice);
        vm.expectRevert(FeeVault.NothingToClaim.selector);
        vault.withdraw(address(p), alice);
    }

    /// Two coins on the same market share one pToken: each coin's holders and
    /// creator are paid from their own books, never from the other's.
    function test_twoCoinsOnOneMarketKeepSeparateBooks() public {
        (Coin c1, BondingCurve k1) = _launch(ID, 10_000);
        (Coin c2, BondingCurve k2) = _launch(ID, 5_000);
        _buy(alice, k1, 2_000e6);
        _buy(bob, k2, 2_000e6);
        vm.warp(block.timestamp + 2 hours);
        PToken p = _p(c1);
        (,, uint256 pot1, uint256 s1) = _book(c1);
        (,, uint256 pot2, uint256 s2) = _book(c2);
        assertEq(p.balanceOf(address(vault)), pot1 + s1 + pot2 + s2 + vault.owed(address(p), creator));
        assertEq(vault.pending(address(c1), bob), 0);
        assertEq(vault.pending(address(c2), alice), 0);
        vm.prank(alice);
        vault.claim(address(c1), alice);
        vm.prank(bob);
        vault.claim(address(c2), bob);
        vm.prank(creator);
        vault.withdraw(address(p), creator);
        (,, pot1, s1) = _book(c1);
        (,, pot2, s2) = _book(c2);
        assertEq(p.balanceOf(address(vault)), pot1 + s1 + pot2 + s2);
    }

    /* ------------------------------------------------ the pool, end to end */

    /// Graduate, trade in the pool, collect, pay: the whole holder path after graduation.
    function test_afterGraduationPoolFeesReachHolders() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 7_000e6);
        assertTrue(curve.graduated());
        vm.warp(block.timestamp + 2 hours);
        address[] memory hs = new address[](3);
        hs[0] = alice;
        hs[1] = platform;
        hs[2] = address(poolManager);
        (PriceOracle.Quote memory q, bytes memory sig) = _none();
        vault.claimFor(address(c), hs, 0, false, q, sig);
        for (uint256 i; i < 6; ++i) {
            (uint256 x,) = _buy(bob, curve, 1_500e6);
            _sell(bob, curve, x);
        }
        graduator.collect(address(c));
        vm.warp(block.timestamp + 2 hours);
        uint256 pa = vault.pending(address(c), alice);
        assertGt(pa, 0);
        assertEq(vault.pending(address(c), address(poolManager)), 0);
        uint256 pp = vault.pending(address(c), platform);
        (q, sig) = _none();
        assertEq(vault.claimFor(address(c), hs, 0, false, q, sig), pa + pp);
        _conserved(c);
    }
}
