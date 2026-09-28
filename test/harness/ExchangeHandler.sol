// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {MockUSDG} from "../Polypad.t.sol";

/**
 * Random actors against the exchange directly: signed mints and redemptions at
 * moving prices, sales over the hourly cap delayed then released or cancelled,
 * the float sent to the desk and refilled, queue payments, frozen recipients
 * withdrawing their set-aside claims, pauses, settlement, time and blocks moving. Ghost books track every USDG and share
 * that moves so the invariants can check the exchange's own accounting.
 */
contract ExchangeHandler is Test {
    PExchange public exchange;
    PriceOracle public oracle;
    MockUSDG public usdg;
    uint256 internal signerPk;
    address internal keeper;
    address internal bridge;

    uint256[2] public ids;
    PToken[2] public tokens;
    address[] public actors;
    /// Where a frozen actor sends its set-aside claim; counts as the actor's.
    mapping(address => address) public sinkOf;
    mapping(address => bool) public frozen;
    uint64[2] public mid;

    // Ghost books, USDG.
    uint256 public ghostUsdgIn; // paid in by minters
    uint256 public ghostOwed; // promised to redeemers and payable now (`out` less any delayed part, plus releases)
    uint256 public ghostDelayed; // promised but held back over the cap, not yet released or cancelled
    uint256 public ghostReceived; // reached actors or their sinks
    uint256 public ghostBridged; // sent to the desk
    uint256 public ghostRefilled; // returned from the desk
    // Ghost books, shares.
    mapping(uint256 => uint256) public ghostMinted;
    mapping(uint256 => uint256) public ghostBurned;
    /// Set if a redemption paid (or queued) more at once than the cap had left.
    bool public ghostOverCap;
    /// Set if `absorb` burned shares held for a delayed sale.
    bool public ghostAbsorbedDelayed;

    uint256 public calls;
    mapping(bytes32 => uint256) public hits;

    function _hit(string memory k) internal {
        hits[keccak256(bytes(k))]++;
    }

    constructor(
        PExchange exchange_,
        PriceOracle oracle_,
        MockUSDG usdg_,
        uint256 signerPk_,
        address keeper_,
        address bridge_,
        uint256[2] memory ids_,
        address[] memory actors_
    ) {
        exchange = exchange_;
        oracle = oracle_;
        usdg = usdg_;
        signerPk = signerPk_;
        keeper = keeper_;
        bridge = bridge_;
        ids = ids_;
        tokens[0] = exchange_.pTokenOf(ids_[0]);
        tokens[1] = exchange_.pTokenOf(ids_[1]);
        actors = actors_;
        mid = [uint64(600_000), uint64(300_000)];
        for (uint256 i; i < actors_.length; ++i) {
            sinkOf[actors_[i]] = address(uint160(uint256(keccak256(abi.encode("sink", i)))));
        }
    }

    function actorCount() external view returns (uint256) {
        return actors.length;
    }

    /* ------------------------------------------------------------ helpers */

    function _sign(PriceOracle.Quote memory q) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signerPk, oracle.quoteDigest(q));
        return abi.encodePacked(r, s, v);
    }

    /// Sum of USDG held by every actor and sink: receipts show up here.
    function _wallets() internal view returns (uint256 sum) {
        for (uint256 i; i < actors.length; ++i) {
            sum += usdg.balanceOf(actors[i]) + usdg.balanceOf(sinkOf[actors[i]]);
        }
    }

    /// Run a call and book whatever reached the actors: wallet delta plus what they spent.
    modifier books() {
        uint256 before = _wallets();
        _;
        ghostReceived += _wallets() + _spent - before;
        _spent = 0;
    }

    uint256 internal _spent;

    function _pick(uint256 seed) internal view returns (address) {
        return actors[seed % actors.length];
    }

    function _price(uint256 m, uint256 seed) internal view returns (uint64) {
        // Around the market's mid, +-4c, clamped inside (0, 1).
        int256 p = int256(uint256(mid[m])) + int256(seed % 80_001) - 40_000;
        if (p < 1) p = 1;
        if (p > 999_999) p = 999_999;
        return uint64(uint256(p));
    }

    /* ------------------------------------------------------------ actions */

    function mint(uint256 who, uint256 m, uint256 amount, uint256 pseed) external books {
        ++calls;
        m %= 2;
        address a = _pick(who);
        if (frozen[a]) return;
        amount = bound(amount, 1, 3_000e6);
        uint64 price = _price(m, pseed);
        PriceOracle.Quote memory q =
            PriceOracle.Quote(ids[m], oracle.BUY(), price, amount, uint64(block.timestamp + 10));
        bytes memory sig = _sign(q);
        vm.prank(a);
        try exchange.mint(address(tokens[m]), amount, 0, a, q, sig) returns (uint256 out) {
            ghostUsdgIn += amount;
            ghostMinted[m] += out;
            _spent = amount;
            _hit("mint");
        } catch {}
    }

    function redeem(uint256 who, uint256 m, uint256 frac, uint256 pseed) external books {
        ++calls;
        m %= 2;
        address a = _pick(who);
        uint256 bal = tokens[m].balanceOf(a);
        if (bal == 0) return;
        uint256 amount = (bal * bound(frac, 1, 100)) / 100;
        if (amount == 0) return;
        PriceOracle.Quote memory q =
            PriceOracle.Quote(ids[m], oracle.SELL(), _price(m, pseed), amount, uint64(block.timestamp + 10));
        bytes memory sig = _sign(q);
        // A frozen actor usually redeems to its sink; sometimes to itself, which
        // only lands when the float is empty and the whole amount queues.
        address to = frozen[a] && frac % 3 != 0 ? sinkOf[a] : a;
        _redeemBooked(a, m, amount, to, q, sig, "redeem");
    }

    /// Redeem and book it, splitting off whatever part was delayed over the cap.
    function _redeemBooked(
        address a,
        uint256 m,
        uint256 amount,
        address to,
        PriceOracle.Quote memory q,
        bytes memory sig,
        string memory tag
    ) internal returns (bool ok) {
        uint256 remaining = exchange.outflowRemaining();
        uint256 n = _delayedLength();
        vm.prank(a);
        try exchange.redeem(address(tokens[m]), amount, 0, to, q, sig) returns (uint256 out) {
            uint256 later;
            uint256 pLater;
            if (_delayedLength() > n) {
                (,,, uint128 pAmount, uint128 amt) = exchange.delayed(n);
                later = amt;
                pLater = pAmount;
                _hit("delayed");
            }
            if (out - later > remaining) ghostOverCap = true;
            ghostOwed += out - later;
            ghostDelayed += later;
            ghostBurned[m] += amount - pLater;
            _hit(tag);
            return true;
        } catch {}
    }

    uint256 public knownDelayed;

    /// Length of `exchange.delayed` (the contract has no length getter): probe past what is known.
    function _delayedLength() internal returns (uint256) {
        while (true) {
            try exchange.delayed(knownDelayed) returns (address, uint64, uint256, uint128, uint128) {
                ++knownDelayed;
            } catch {
                return knownDelayed;
            }
        }
        return knownDelayed;
    }

    function release(uint256 ticket) external books {
        ++calls;
        uint256 n = _delayedLength();
        if (n == 0) return;
        ticket %= n;
        (address to,, uint256 id, uint128 pAmount, uint128 amt) = exchange.delayed(ticket);
        if (to == address(0)) return;
        try exchange.release(ticket) {
            ghostDelayed -= amt;
            ghostOwed += amt;
            ghostBurned[id == ids[0] ? 0 : 1] += pAmount;
            _hit("release");
        } catch {}
    }

    function cancel(uint256 ticket) external {
        ++calls;
        uint256 n = _delayedLength();
        if (n == 0) return;
        ticket %= n;
        (address to,,,, uint128 amt) = exchange.delayed(ticket);
        if (to == address(0)) return;
        vm.prank(exchange.owner());
        exchange.cancel(ticket);
        ghostDelayed -= amt;
        _hit("cancel");
    }

    function absorb(uint256 m) external {
        ++calls;
        m %= 2;
        uint256 got = exchange.absorb(address(tokens[m]));
        if (got > 0) ghostAbsorbedDelayed = true;
    }

    function transferShares(uint256 from, uint256 to, uint256 m, uint256 frac) external {
        ++calls;
        m %= 2;
        address a = _pick(from);
        address b = _pick(to);
        uint256 amount = (tokens[m].balanceOf(a) * bound(frac, 0, 100)) / 100;
        vm.prank(a);
        tokens[m].transfer(b, amount);
    }

    /// Move a market's mid: the price the pricer signs around.
    function post(uint256 m, uint256 move) external {
        ++calls;
        m %= 2;
        // Mostly small moves, sometimes a jump.
        int256 d = int256(move % 70_001) - 35_000;
        int256 p = int256(uint256(mid[m])) + d;
        if (p < 20_000) p = 20_000;
        if (p > 980_000) p = 980_000;
        mid[m] = uint64(uint256(p));
        _hit("post");
    }

    function payQueue(uint256 max) external books {
        ++calls;
        exchange.payQueue(bound(max, 0, 8));
        _hit("payQueue");
    }

    function toDesk(uint256 frac) external {
        ++calls;
        uint256 amount = (exchange.freeFloat() * bound(frac, 0, 100)) / 100;
        vm.prank(keeper);
        exchange.sendToBridge(amount);
        ghostBridged += amount;
    }

    function fromDesk(uint256 amount) external {
        ++calls;
        amount = bound(amount, 0, 5_000e6);
        usdg.mint(address(exchange), amount);
        ghostRefilled += amount;
    }

    function freeze(uint256 who) external {
        ++calls;
        address a = _pick(who);
        // Keep at least two healthy actors.
        uint256 n;
        for (uint256 i; i < actors.length; ++i) {
            if (frozen[actors[i]]) ++n;
        }
        if (frozen[a] || n + 2 >= actors.length) return;
        frozen[a] = true;
        usdg.freeze(a);
        _hit("freeze");
    }

    /// The path to a set-aside claim, in one step: an actor sells with the float
    /// at the desk (so the sale queues), then gets frozen before it is paid.
    function queueThenFreeze(uint256 who, uint256 m) external books {
        ++calls;
        m %= 2;
        address a = _pick(who);
        if (frozen[a] || tokens[m].balanceOf(a) == 0) return;
        uint256 free = exchange.freeFloat();
        vm.prank(keeper);
        exchange.sendToBridge(free);
        ghostBridged += free;
        uint256 amount = tokens[m].balanceOf(a) / 2;
        if (amount == 0) return;
        PriceOracle.Quote memory q =
            PriceOracle.Quote(ids[m], oracle.SELL(), mid[m], amount, uint64(block.timestamp + 10));
        bytes memory sig = _sign(q);
        if (!_redeemBooked(a, m, amount, a, q, sig, "queueThenFreeze")) return;
        this.freeze(who);
    }

    function withdrawUnclaimed(uint256 who) external books {
        ++calls;
        address a = _pick(who);
        if (exchange.unclaimed(a) == 0) return;
        vm.prank(a);
        try exchange.withdrawUnclaimed(sinkOf[a]) {
            _hit("withdrawUnclaimed");
        } catch {}
    }

    function report(uint256 m, uint256 amount) external {
        ++calls;
        m %= 2;
        uint256[] memory i1 = new uint256[](1);
        uint256[] memory a1 = new uint256[](1);
        i1[0] = ids[m];
        a1[0] = bound(amount, 0, 100_000e6);
        vm.prank(keeper);
        exchange.reportBacked(i1, a1);
    }

    function pause(uint256 m, uint256 seed) external {
        ++calls;
        bool on = seed % 4 == 0; // mostly unpausing, so markets stay open
        uint256[] memory i1 = new uint256[](1);
        i1[0] = ids[m % 2];
        vm.prank(keeper);
        oracle.setPaused(i1, on);
    }

    function settle(uint256 m, uint256 payoutSeed) external {
        ++calls;
        m %= 2;
        (, bool settled,) = oracle.status(ids[m]);
        if (settled) return;
        // Only rarely: settling ends a market's pricing for the rest of the run.
        if (payoutSeed % 40 != 0) return;
        uint64 payout = [uint64(0), 1e6, 500_000][(payoutSeed / 40) % 3];
        vm.prank(keeper);
        oracle.settle(ids[m], payout);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        _hit("settle");
    }

    function warp(uint256 secs) external {
        ++calls;
        vm.warp(block.timestamp + (secs % 17 == 0 ? bound(secs, 0, 2 hours) : bound(secs, 0, 5 minutes)));
        vm.roll(block.number + 1);
    }

    function roll() external {
        ++calls;
        vm.roll(block.number + 1);
    }
}
