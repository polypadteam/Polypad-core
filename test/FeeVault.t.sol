// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../src/BondingCurve.sol";
import {Coin} from "../src/Coin.sol";
import {FeeVault} from "../src/FeeVault.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {PToken} from "../src/PToken.sol";

import {PolypadBase} from "./Polypad.t.sol";

/// @dev Creator fee claims and the holder dividend.
contract FeeVaultTest is PolypadBase {
    address internal carol = makeAddr("carol");

    function setUp() public override {
        super.setUp();
        usdg.mint(carol, 100_000e6);
        vm.prank(carol);
        usdg.approve(address(router), type(uint256).max);
        vm.prank(owner);
        exchange.setMaxRisk(ID, type(uint256).max);
    }

    function _pOf(Coin c) internal view returns (PToken) {
        (,, address p,,) = vault.launches(address(c));
        return PToken(p);
    }

    /// @dev The vault holds at least everything it owes: creators, the pot and the stream.
    function _assertSolvent(Coin c, address[] memory holders) internal view {
        PToken p = _pOf(c);
        (,, uint256 pot, uint256 streaming,,) = vault.books(address(c));
        uint256 owedCreator = vault.owed(address(p), creator);
        assertGe(p.balanceOf(address(vault)), owedCreator + pot + streaming, "vault short");
        uint256 promised;
        for (uint256 i; i < holders.length; ++i) {
            promised += vault.pending(address(c), holders[i]);
        }
        assertLe(promised, pot + streaming, "promised more than it holds");
    }

    /* ------------------------------------------------------------ creator mode */

    function test_creatorModeCreditsTheCreatorAndPaysOutInUsdg() public {
        (Coin c, BondingCurve curve) = _launch(ID);
        assertEq(c.holderBook(), address(0)); // a plain ERC20, no transfer hook
        _buy(alice, curve, 1_000e6);
        PToken p = _pOf(c);
        uint256 owed = vault.owed(address(p), creator);
        assertGt(owed, 0);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 before = usdg.balanceOf(creator);
        vm.prank(creator);
        uint256 out = vault.withdrawUsd(address(p), creator, 0, q, sig);
        assertEq(usdg.balanceOf(creator) - before, out);
        assertApproxEqRel(out, (owed * 600_000 / 1e6) * 9975 / 10_000, 0.001e18);
        assertEq(vault.owed(address(p), creator), 0);

        vm.expectRevert(FeeVault.NothingToClaim.selector);
        vm.prank(creator);
        vault.withdraw(address(p), creator);
    }

    function test_payeeCanHandOverFutureFees() public {
        (Coin c, BondingCurve curve) = _launch(ID);
        PToken p = _pOf(c);
        _buy(alice, curve, 500e6);
        uint256 early = vault.owed(address(p), creator);

        vm.expectRevert(FeeVault.OnlyPayee.selector);
        vault.setPayee(address(c), bob);
        vm.prank(creator);
        vault.setPayee(address(c), carol);

        _buy(alice, curve, 500e6);
        assertEq(vault.owed(address(p), creator), early); // what was owed stays theirs
        assertGt(vault.owed(address(p), carol), 0);
    }

    function test_onlyTheFactoryRegisters() public {
        vm.expectRevert(FeeVault.OnlyFactory.selector);
        vault.register(address(1), address(2), address(3), address(4), creator, 0);
    }

    function test_launchRefusesAHolderShareOver100Percent() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.expectRevert();
        factory.launch(ID, "X", "X", "", 10_001, q, sig);
    }

    /* ------------------------------------------------------------- holder mode */

    function test_holdersShareTheStreamByBalance() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        assertEq(c.holderBook(), address(vault));
        (uint256 a,) = _buy(alice, curve, 1_000e6);
        vm.warp(block.timestamp + 1);
        (uint256 b,) = _buy(bob, curve, 1_000e6);
        PToken p = _pOf(c);
        assertEq(vault.owed(address(p), creator), 0); // all of it to holders

        // Everything deposited streams out within the hour, by balance.
        vm.warp(block.timestamp + 1 hours);
        uint256 pa = vault.pending(address(c), alice);
        uint256 pb = vault.pending(address(c), bob);
        assertGt(pb, 0);
        (,,, uint256 streaming,,) = vault.books(address(c));
        // Bob's buy fee streamed over the hour to both; alice also had alice's own fee for 1s alone.
        assertApproxEqRel(pa * b, pb * a, 0.02e18);
        assertGt(pa + pb, 0);
        assertLe(pa + pb, streaming + pa + pb);

        vm.prank(alice);
        uint256 got = vault.claim(address(c), alice);
        assertEq(got, pa);
        assertEq(p.balanceOf(alice), pa);

        address[] memory hs = new address[](2);
        hs[0] = alice;
        hs[1] = bob;
        _assertSolvent(c, hs);
    }

    function test_halfToHoldersHalfToCreator() public {
        (Coin c, BondingCurve curve) = _launch(ID, 5_000);
        _buy(alice, curve, 1_000e6);
        vm.warp(block.timestamp + 2 hours);
        PToken p = _pOf(c);
        uint256 toCreator = vault.owed(address(p), creator);
        uint256 toHolders = vault.pending(address(c), alice);
        // Alice held nothing when her own buy's fee was deposited, but she is the
        // only holder while it streams, so she receives all of the holders' half.
        assertApproxEqAbs(toHolders, toCreator, 2);
    }

    function test_aSellerStopsEarningAtTheSale() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        (uint256 a,) = _buy(alice, curve, 1_000e6);
        (uint256 b,) = _buy(bob, curve, 1_000e6);
        _sell(alice, curve, a);
        uint256 frozen = vault.pending(address(c), alice);
        vm.warp(block.timestamp + 1 hours);
        // Carol trades after alice left: alice earns nothing more.
        _buy(carol, curve, 2_000e6);
        vm.warp(block.timestamp + 1 hours);
        assertEq(vault.pending(address(c), alice), frozen);
        assertGt(vault.pending(address(c), bob), 0);
        b;
    }

    function test_aFlashHolderCapturesNothing() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 1_000e6);
        vm.warp(block.timestamp + 10 minutes);
        _buy(bob, curve, 500e6);
        // Carol buys a big bag and sells it in the same block.
        (uint256 got,) = _buy(carol, curve, 3_000e6);
        _sell(carol, curve, got);
        assertEq(vault.pending(address(c), carol), 0);
    }

    function test_poolFeesCannotBeSnipedAroundCollect() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        (uint256 a,) = _buy(alice, curve, 7_000e6); // graduates
        assertTrue(curve.graduated());
        vm.warp(block.timestamp + 2 hours);
        // A day of pool volume, fees sitting in the position.
        for (uint256 i; i < 5; ++i) {
            (uint256 x,) = _buy(bob, curve, 2_000e6);
            _sell(bob, curve, x);
        }
        vm.warp(block.timestamp + 2 hours);
        uint256 alicePending = vault.pending(address(c), alice);

        // Carol buys right before the collect and sells right after.
        (uint256 got,) = _buy(carol, curve, 5_000e6);
        uint256 deadBefore = c.balanceOf(curve.DEAD());
        graduator.collect(address(c));
        assertGt(c.balanceOf(curve.DEAD()), deadBefore); // the holders' coin fees are burned
        _sell(carol, curve, got);
        assertEq(vault.pending(address(c), carol), 0);

        // The collected pToken streams to whoever holds through the next hour.
        vm.warp(block.timestamp + 1 hours);
        assertGt(vault.pending(address(c), alice), alicePending);
        a;
    }

    function test_excludedAddressesNeverEarn() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 7_000e6); // graduates: the pool holds coins
        vm.warp(block.timestamp + 1 hours);
        assertEq(vault.pending(address(c), address(curve)), 0);
        assertEq(vault.pending(address(c), address(poolManager)), 0);
        assertEq(vault.pending(address(c), curve.DEAD()), 0);
        assertEq(vault.tracked(address(c), address(poolManager)), 0);
        // Eligible supply is exactly what wallets hold.
        (, uint256 supply,,,,) = vault.books(address(c));
        // Alice, and the platform holding the graduation rounding dust.
        assertEq(supply, c.balanceOf(alice) + c.balanceOf(platform));
    }

    function test_aHolderCashesOutTheirOwnRewardsOthersGetShares() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        _buy(bob, curve, 1_000e6);
        vm.warp(block.timestamp + 1 hours);
        uint256 pa = vault.pending(address(c), alice);
        uint256 pb = vault.pending(address(c), bob);

        address[] memory hs = new address[](3);
        hs[0] = alice;
        hs[1] = bob;
        hs[2] = carol; // owed nothing: skipped, not reverted
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        uint256 ua = usdg.balanceOf(alice);
        uint256 ub = usdg.balanceOf(bob);
        // The keeper asks for USDG, but may not force a sale on anyone: shares it is.
        vm.prank(keeper);
        uint256 paid = vault.claimFor(address(c), hs, 0, true, q, sig);
        assertEq(paid, pa + pb);
        assertEq(usdg.balanceOf(alice), ua);
        assertEq(_pOf(c).balanceOf(alice), pa);
        assertEq(_pOf(c).balanceOf(bob), pb);

        // A holder paying themselves may cash out.
        vm.warp(block.timestamp + 1 hours);
        _buy(carol, curve, 500e6);
        vm.warp(block.timestamp + 1 hours);
        uint256 pa2 = vault.pending(address(c), alice);
        address[] memory me = new address[](1);
        me[0] = alice;
        (q, sig) = signedQuote(ID, SELL);
        vm.prank(alice);
        assertEq(vault.claimFor(address(c), me, 0, true, q, sig), pa2);
        assertApproxEqRel(usdg.balanceOf(alice) - ua, (pa2 * 600_000 / 1e6) * 9975 / 10_000, 0.001e18);
        assertEq(usdg.balanceOf(bob), ub);
        _assertSolvent(c, hs);
    }

    function test_claimForSkipsSmallAndRefusedPayments() public {
        (Coin c, BondingCurve curve) = _launch(ID, 10_000);
        _buy(alice, curve, 2_000e6);
        vm.warp(block.timestamp + 1 hours);
        uint256 pa = vault.pending(address(c), alice);
        address[] memory hs = new address[](1);
        hs[0] = alice;
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);

        // Below the minimum: untouched.
        vm.prank(alice);
        assertEq(vault.claimFor(address(c), hs, pa + 1, true, q, sig), 0);
        assertEq(vault.pending(address(c), alice), pa);

        // The exchange refuses (halted): skipped, rewards kept.
        vm.prank(keeper);
        exchange.halt();
        vm.prank(alice);
        assertEq(vault.claimFor(address(c), hs, 0, true, q, sig), 0);
        assertEq(vault.pending(address(c), alice), pa);

        // Paid in pToken instead.
        assertEq(vault.claimFor(address(c), hs, 0, false, q, sig), pa);
        assertEq(_pOf(c).balanceOf(alice), pa);
    }

    function test_holdersAcrossTransfersStaySolvent() public {
        (Coin c, BondingCurve curve) = _launch(ID, 7_500);
        address[] memory hs = new address[](3);
        hs[0] = alice;
        hs[1] = bob;
        hs[2] = carol;
        for (uint256 i; i < 12; ++i) {
            address who = hs[i % 3];
            (uint256 got,) = _buy(who, curve, 150e6 + i * 37e6);
            vm.warp(block.timestamp + 7 minutes);
            vm.prank(who);
            c.transfer(hs[(i + 1) % 3], got / 3);
            if (i % 4 == 3) _sell(hs[(i + 2) % 3], curve, c.balanceOf(hs[(i + 2) % 3]) / 2);
            _assertSolvent(c, hs);
        }
        vm.warp(block.timestamp + 2 hours);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vault.claimFor(address(c), hs, 0, false, q, sig);
        _assertSolvent(c, hs);
        // After the stream ends and everyone is paid, only flooring dust is left behind.
        (,, uint256 pot, uint256 streaming,,) = vault.books(address(c));
        assertLe(pot + streaming, 1_000);
    }

    /// forge-config: default.fuzz.runs = 64
    function testFuzz_solventUnderRandomActivity(uint256 seed, uint16 holdersBps) public {
        holdersBps = uint16(bound(holdersBps, 1, 10_000));
        (Coin c, BondingCurve curve) = _launch(ID, holdersBps);
        address[] memory hs = new address[](3);
        hs[0] = alice;
        hs[1] = bob;
        hs[2] = carol;
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        for (uint256 i; i < 16; ++i) {
            uint256 r = uint256(keccak256(abi.encode(seed, i)));
            address who = hs[r % 3];
            uint256 action = (r >> 8) % 5;
            if (action == 0 || c.balanceOf(who) == 0) {
                _buy(who, curve, 20e6 + (r >> 16) % 1_500e6);
            } else if (action == 1) {
                _sell(who, curve, c.balanceOf(who) * ((r >> 16) % 100 + 1) / 100);
            } else if (action == 2) {
                uint256 half = c.balanceOf(who) / 2;
                vm.prank(who);
                c.transfer(hs[(r >> 16) % 3], half);
            } else if (action == 3) {
                if (vault.pending(address(c), who) > 0) {
                    vm.prank(who);
                    vault.claim(address(c), who);
                }
            } else {
                q.validUntil = uint64(block.timestamp + 15);
                sig = _sign(q, signerPk);
                vault.claimFor(address(c), hs, 0, (r >> 16) % 2 == 0, q, sig);
            }
            if (curve.graduated() && (r >> 24) % 3 == 0) graduator.collect(address(c));
            vm.warp(block.timestamp + (r >> 32) % 30 minutes);
            _assertSolvent(c, hs);
        }
    }
}
