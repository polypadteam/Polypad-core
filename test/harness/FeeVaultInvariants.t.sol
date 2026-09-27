// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {FeeVault} from "../../src/FeeVault.sol";
import {Graduator} from "../../src/Graduator.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {Router} from "../../src/Router.sol";
import {PolypadBase, MockUSDG} from "../Polypad.t.sol";

/**
 * @dev Drives one holder-share coin through everything that can touch its
 *      holder books: curve and pool trades, graduation, transfers to wallets and
 *      to every excluded address, time, claims, batched claims (with duplicates,
 *      excluded addresses, minimums, cash-outs by the holder and by strangers),
 *      pool fee collection, creator withdrawals, payee changes and donations.
 *
 *      Reverts that are the guards working are fine. What must never happen is
 *      recorded in a flag: a valid coin transfer reverting (a frozen coin), a
 *      holder's pending falling without them being paid, a stranger cashing a
 *      holder out, or a claim with something pending reverting.
 */
contract FeeVaultHandler is Test {
    PolypadBase internal base;
    Router internal router;
    BondingCurve internal curve;
    Coin internal coin;
    PToken internal p;
    FeeVault internal vault;
    Graduator internal graduator;
    PExchange internal exchange;
    MockUSDG internal usdg;
    address internal poolManager;
    address internal platform;

    address[] public actors;
    address[] public payees;
    address public payee; // current payee
    address internal stranger = makeAddr("stranger");

    // ghosts
    uint256 public coinDonatedToVault;
    bool public transferFroze;
    string public transferFrozeWhy;
    bool public pendingDropped;
    string public pendingDroppedWhy;
    bool public foreignCashOut;
    bool public claimFailed;
    bool public claimMismatch;
    uint256 public holderPaid;
    mapping(bytes32 => uint256) public calls;

    constructor(
        PolypadBase base_,
        Router router_,
        BondingCurve curve_,
        FeeVault vault_,
        Graduator graduator_,
        PExchange exchange_,
        MockUSDG usdg_,
        address poolManager_,
        address platform_,
        address[] memory actors_,
        address[] memory payees_
    ) {
        base = base_;
        router = router_;
        curve = curve_;
        coin = Coin(address(curve_.coin()));
        p = PToken(address(curve_.pToken()));
        vault = vault_;
        graduator = graduator_;
        exchange = exchange_;
        usdg = usdg_;
        poolManager = poolManager_;
        platform = platform_;
        actors = actors_;
        payees = payees_;
        payee = payees_[0];
    }

    function actorCount() external view returns (uint256) {
        return actors.length;
    }

    function payeeCount() external view returns (uint256) {
        return payees.length;
    }

    /* ------------------------------------------------------ monotone check */

    /// Everyone who can hold coins and earn.
    function _earners() internal view returns (address[] memory e) {
        e = new address[](actors.length + payees.length + 2);
        uint256 n;
        for (uint256 i; i < actors.length; ++i) e[n++] = actors[i];
        for (uint256 i; i < payees.length; ++i) e[n++] = payees[i];
        e[n++] = platform;
        e[n++] = address(router);
    }

    struct Snap {
        uint256[] pending;
        uint256[] pBal;
    }

    function _snap() internal view returns (Snap memory s) {
        address[] memory e = _earners();
        s.pending = new uint256[](e.length);
        s.pBal = new uint256[](e.length);
        for (uint256 i; i < e.length; ++i) {
            s.pending[i] = vault.pending(address(coin), e[i]);
            s.pBal[i] = p.balanceOf(e[i]);
        }
    }

    address internal skipCheck; // an address whose pending may legitimately drop this call (it cashed out)

    modifier monotone(bytes32 name) {
        calls[name]++;
        Snap memory before = _snap();
        _;
        address[] memory e = _earners();
        for (uint256 i; i < e.length; ++i) {
            if (e[i] == skipCheck) continue;
            uint256 nowPending = vault.pending(address(coin), e[i]);
            uint256 got = p.balanceOf(e[i]) - before.pBal[i];
            // A payee withdrawing creator pToken also raises their pToken balance: that only loosens this.
            if (nowPending + got + 2 < before.pending[i]) {
                pendingDropped = true;
                pendingDroppedWhy = string(
                    abi.encodePacked(
                        vm.toString(name), " ", vm.toString(e[i]), " ", vm.toString(before.pending[i]), "->", vm.toString(nowPending)
                    )
                );
            }
        }
        skipCheck = address(0);
    }

    /* ------------------------------------------------------------- actions */

    function buy(uint256 who, uint256 amount) external monotone("buy") {
        address t = actors[who % actors.length];
        amount = bound(amount, 1e6, 3_000e6);
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 0);
        vm.prank(t);
        try router.buy(curve, amount, 0, t, q, sig) {} catch {}
    }

    /// Enough to sell out the curve and graduate it.
    function bigBuy(uint256 who) external monotone("bigBuy") {
        address t = actors[who % actors.length];
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 0);
        vm.prank(t);
        try router.buy(curve, 7_000e6, 0, t, q, sig) {} catch {}
    }

    function sell(uint256 who, uint256 fraction) external monotone("sell") {
        address t = actors[who % actors.length];
        uint256 bal = coin.balanceOf(t);
        if (bal == 0) return;
        uint256 amount = (bal * bound(fraction, 1, 100)) / 100;
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 1);
        vm.startPrank(t);
        coin.approve(address(router), amount);
        try router.sell(curve, amount, 0, t, q, sig) {} catch {}
        vm.stopPrank();
    }

    function _dest(uint256 k, address from) internal view returns (address) {
        uint256 n = actors.length;
        k = k % (n + 8);
        if (k < n) return actors[k];
        if (k == n) return vault.DEAD();
        if (k == n + 1) return poolManager;
        if (k == n + 2) return address(vault);
        if (k == n + 3) return address(graduator);
        if (k == n + 4) return address(curve);
        if (k == n + 5) return from; // to self
        if (k == n + 6) return platform;
        return stranger;
    }

    /// A plain transfer of any size (zero included) to anyone. It must never revert.
    function transfer(uint256 who, uint256 dest, uint256 fraction) external monotone("transfer") {
        address from = actors[who % actors.length];
        address to = _dest(dest, from);
        uint256 amount = (coin.balanceOf(from) * bound(fraction, 0, 100)) / 100;
        vm.prank(from);
        try coin.transfer(to, amount) {
            if (to == address(vault) && from != to) coinDonatedToVault += amount;
        } catch (bytes memory reason) {
            transferFroze = true;
            transferFrozeWhy = string(abi.encodePacked("transfer ", vm.toString(from), "->", vm.toString(to), " ", reason));
        }
    }

    /// A stranger moving someone's coins with an allowance, and zero-value transfers from empty wallets.
    function transferFromEmpty(uint256 dest) external monotone("transferFromEmpty") {
        address to = _dest(dest, stranger);
        vm.prank(stranger);
        try coin.transfer(to, 0) {}
        catch (bytes memory reason) {
            transferFroze = true;
            transferFrozeWhy = string(abi.encodePacked("zero transfer ", reason));
        }
    }

    function warp(uint256 dt) external monotone("warp") {
        dt = bound(dt, 0, 2 hours);
        vm.warp(block.timestamp + dt);
        vm.roll(block.number + dt / 2 + 1);
    }

    function claim(uint256 who) external monotone("claim") {
        address t = actors[who % actors.length];
        uint256 pend = vault.pending(address(coin), t);
        vm.prank(t);
        try vault.claim(address(coin), t) returns (uint256 got) {
            holderPaid += got;
            // The view is what a claim pays, to the wei.
            if (got != pend) claimMismatch = true;
        } catch {
            if (pend > 0) claimFailed = true;
        }
    }

    function claimFor(uint256 caller, uint256 seed, uint256 minAmount, bool cashOut) external monotone("claimFor") {
        uint256 n = bound(seed, 1, 8);
        address[] memory hs = new address[](n);
        for (uint256 i; i < n; ++i) {
            uint256 r = uint256(keccak256(abi.encode(seed, i)));
            // Mostly holders, with duplicates and excluded addresses mixed in.
            hs[i] = r % 5 == 0 ? _dest(r >> 8, actors[0]) : actors[(r >> 16) % actors.length];
        }
        address from = caller % 3 == 0 ? stranger : actors[caller % actors.length];
        minAmount = bound(minAmount, 0, 1e6);
        uint256[] memory usdBefore = new uint256[](n);
        for (uint256 i; i < n; ++i) usdBefore[i] = usdg.balanceOf(hs[i]);
        (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 1);
        skipCheck = from;
        vm.prank(from);
        try vault.claimFor(address(coin), hs, minAmount, cashOut, q, sig) returns (uint256 paid) {
            holderPaid += paid;
        } catch {
            claimFailed = true; // claimFor skips, it never reverts for a known coin
        }
        for (uint256 i; i < n; ++i) {
            if (hs[i] != from && usdg.balanceOf(hs[i]) > usdBefore[i]) foreignCashOut = true;
        }
    }

    function collect() external monotone("collect") {
        if (!curve.graduated()) return;
        graduator.collect(address(coin));
    }

    function creatorWithdraw(uint256 mode) external monotone("creatorWithdraw") {
        mode = mode % 3;
        if (mode == 0) {
            if (vault.owed(address(p), payee) == 0) return;
            vm.prank(payee);
            vault.withdraw(address(p), payee);
        } else if (mode == 1) {
            if (vault.owed(address(coin), payee) == 0) return;
            vm.prank(payee);
            vault.withdraw(address(coin), payee);
        } else {
            if (vault.owed(address(p), payee) == 0) return;
            (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 1);
            vm.prank(payee);
            try vault.withdrawUsd(address(p), payee, 0, q, sig) {} catch {}
        }
    }

    function rotatePayee(uint256 k) external monotone("rotatePayee") {
        address next = payees[k % payees.length];
        vm.prank(payee);
        vault.setPayee(address(coin), next);
        payee = next;
    }

    /// Anyone may pay into the vault for a coin: pToken (streams) or the coin (burned / owed).
    function donate(uint256 amount, bool inCoin) external monotone("donate") {
        if (inCoin) {
            address t = actors[amount % actors.length];
            uint256 bal = coin.balanceOf(t);
            if (bal == 0) return;
            amount = bound(amount, 0, bal);
            vm.startPrank(t);
            coin.approve(address(vault), amount);
            vault.deposit(address(coin), address(coin), amount);
            vm.stopPrank();
        } else {
            amount = bound(amount, 0, 50e6);
            if (amount == 0) return;
            usdg.mint(address(this), amount);
            usdg.approve(address(exchange), amount);
            (PriceOracle.Quote memory q, bytes memory sig) = base.signedQuote(0xFED, 0);
            uint256 shares = exchange.mint(address(p), amount, 0, address(this), q, sig);
            p.approve(address(vault), shares);
            vault.deposit(address(coin), address(p), shares);
        }
    }

    /// Drop eligible supply toward zero: a holder burns most of their bag.
    function burnToDead(uint256 who) external monotone("burnToDead") {
        address t = actors[who % actors.length];
        uint256 bal = coin.balanceOf(t);
        address dead = vault.DEAD();
        vm.prank(t);
        try coin.transfer(dead, bal - bal / 1e9) {}
        catch (bytes memory reason) {
            transferFroze = true;
            transferFrozeWhy = string(abi.encodePacked("burn ", reason));
        }
    }
}

contract FeeVaultInvariants is PolypadBase {
    FeeVaultHandler internal handler;
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;

    function setUp() public override {
        super.setUp();
        vm.prank(owner);
        exchange.setMaxUnbacked(ID, type(uint256).max);
        usdg.mint(address(exchange), 10_000_000e6);
        (coin, curve) = _launch(ID, 7_000);
        p = exchange.pTokenOf(ID);

        address[] memory actors = new address[](4);
        for (uint256 i; i < 4; ++i) {
            actors[i] = makeAddr(string(abi.encodePacked("holder", vm.toString(i))));
            usdg.mint(actors[i], 10_000_000e6);
            vm.prank(actors[i]);
            usdg.approve(address(router), type(uint256).max);
        }
        address[] memory payees = new address[](3);
        payees[0] = creator;
        payees[1] = makeAddr("payee1");
        payees[2] = makeAddr("payee2");

        handler = new FeeVaultHandler(
            this, router, curve, vault, graduator, exchange, usdg, address(poolManager), platform, actors, payees
        );
        targetContract(address(handler));
    }

    function _owedAll(address asset) internal view returns (uint256 s) {
        for (uint256 i; i < handler.payeeCount(); ++i) s += vault.owed(asset, handler.payees(i));
    }

    /// Every pToken the vault holds is accounted for, exactly: creators, the pot and the stream.
    /// forge-config: default.invariant.runs = 96
    /// forge-config: default.invariant.depth = 60
    function invariant_pTokenConserved() public view {
        (,, uint256 pot, uint256 streaming,,) = vault.books(address(coin));
        assertEq(p.balanceOf(address(vault)), pot + streaming + _owedAll(address(p)), "pToken books");
    }

    /// Coins in the vault are creator fees owed, plus whatever was sent to it by mistake.
    /// forge-config: default.invariant.runs = 96
    /// forge-config: default.invariant.depth = 60
    function invariant_coinConserved() public view {
        assertEq(coin.balanceOf(address(vault)), _owedAll(address(coin)) + handler.coinDonatedToVault(), "coin books");
    }

    /// The vault never promises holders more than it has for them.
    /// forge-config: default.invariant.runs = 96
    /// forge-config: default.invariant.depth = 60
    function invariant_promisesCovered() public view {
        (,, uint256 pot, uint256 streaming,,) = vault.books(address(coin));
        uint256 promised;
        for (uint256 i; i < handler.actorCount(); ++i) promised += vault.pending(address(coin), handler.actors(i));
        for (uint256 i; i < handler.payeeCount(); ++i) promised += vault.pending(address(coin), handler.payees(i));
        promised += vault.pending(address(coin), platform);
        promised += vault.pending(address(coin), address(router));
        assertLe(promised, pot + streaming, "over-promised");
        // Rewards already banked are always payable from the pot.
        uint256 banked;
        for (uint256 i; i < handler.actorCount(); ++i) banked += vault.rewards(address(coin), handler.actors(i));
        assertLe(banked, pot + 2, "banked beyond pot");
    }

    /// The tracked supply is exactly what non-excluded addresses hold.
    /// forge-config: default.invariant.runs = 96
    /// forge-config: default.invariant.depth = 60
    function invariant_supplyIsWallets() public view {
        (, uint256 supply,,,,) = vault.books(address(coin));
        uint256 excludedHeld = coin.balanceOf(address(curve)) + coin.balanceOf(address(poolManager))
            + coin.balanceOf(vault.DEAD()) + coin.balanceOf(address(vault)) + coin.balanceOf(address(graduator));
        assertEq(supply, coin.SUPPLY() - excludedHeld, "tracked supply");
        for (uint256 i; i < handler.actorCount(); ++i) {
            address a = handler.actors(i);
            assertEq(vault.tracked(address(coin), a), coin.balanceOf(a), "tracked != balance");
        }
    }

    /// The pool, the curve, the Graduator, the vault and the burn address never earn.
    /// forge-config: default.invariant.runs = 96
    /// forge-config: default.invariant.depth = 60
    function invariant_excludedNeverAccrue() public view {
        address[5] memory ex = [address(curve), address(poolManager), address(graduator), address(vault), vault.DEAD()];
        for (uint256 i; i < ex.length; ++i) {
            assertEq(vault.tracked(address(coin), ex[i]), 0, "excluded tracked");
            assertEq(vault.rewards(address(coin), ex[i]), 0, "excluded rewarded");
            assertEq(vault.pending(address(coin), ex[i]), 0, "excluded pending");
        }
    }

    /// forge-config: default.invariant.runs = 96
    /// forge-config: default.invariant.depth = 60
    function invariant_noViolations() public view {
        assertFalse(handler.transferFroze(), handler.transferFrozeWhy());
        assertFalse(handler.pendingDropped(), handler.pendingDroppedWhy());
        assertFalse(handler.foreignCashOut(), "a stranger cashed a holder out");
        assertFalse(handler.claimFailed(), "a claim with rewards pending reverted");
        assertFalse(handler.claimMismatch(), "claim paid something other than pending()");
    }
}
