// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Vm} from "forge-std/Vm.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {BondingCurve} from "../../src/BondingCurve.sol";
import {Coin} from "../../src/Coin.sol";
import {FeeVault} from "../../src/FeeVault.sol";
import {Graduator} from "../../src/Graduator.sol";
import {LaunchFactory} from "../../src/LaunchFactory.sol";
import {PExchange} from "../../src/PExchange.sol";
import {PriceOracle} from "../../src/PriceOracle.sol";
import {PToken} from "../../src/PToken.sol";
import {Router} from "../../src/Router.sol";
import {Deploy} from "../../script/Deploy.s.sol";
import {PolypadBase} from "../Polypad.t.sol";

contract RunForwarder {
    function run(Deploy d) external {
        d.run();
    }
}

/// @dev Launches from a contract, the way an aggregator or a launch bot would.
contract LaunchBot {
    function launch(LaunchFactory f, uint256 id, PriceOracle.Quote calldata q, bytes calldata sig)
        external
        returns (Coin, BondingCurve)
    {
        return f.launch(id, "Bot", "BOT", "", 0, q, sig);
    }
}

/**
 * Launching: which markets, which quotes, who, how often, what gets emitted,
 * and the deployment script's wiring.
 */
contract LaunchFactoryHarnessTest is PolypadBase {
    /// @dev Forge tests do not enforce EIP-170, a real deployment does: v8's first
    ///      deploy attempt stopped at a PExchange 72 bytes over the limit.
    function test_everyContractFitsTheCodeSizeLimit() public view {
        assertLe(address(oracle).code.length, 24_576, "PriceOracle");
        assertLe(address(exchange).code.length, 24_576, "PExchange");
        assertLe(address(factory).code.length, 24_576, "LaunchFactory");
        assertLe(address(router).code.length, 24_576, "Router");
        assertLe(address(graduator).code.length, 24_576, "Graduator");
        assertLe(address(vault).code.length, 24_576, "FeeVault");
    }

    bytes32 internal constant LAUNCHED = keccak256("Launched(uint256,address,address,address,address,uint256,uint256,uint16)");

    function _quote(uint256 id, uint8 side, uint64 price) internal view returns (PriceOracle.Quote memory q, bytes memory sig) {
        q = PriceOracle.Quote(id, side, price, type(uint256).max, uint64(block.timestamp + 10));
        sig = _sign(q, signerPk);
    }

    /* ------------------------------------------------------- which markets */

    function test_cannotLaunchOnAPausedMarket() public {
        _pause(ID);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        factory.launch(ID, "a", "A", "", 0, q, sig);
    }

    function test_cannotLaunchOnASettledMarket() public {
        vm.prank(keeper);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketSettled.selector, ID));
        factory.launch(ID, "a", "A", "", 0, q, sig);
    }

    function test_cannotLaunchWhileASettlementIsPending() public {
        vm.prank(keeper);
        oracle.settle(ID, 1e6);
        // Recorded, not in effect: the market is paused for buys, launches included.
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        factory.launch(ID, "a", "A", "", 0, q, sig);
    }

    function test_launchPriceBandEdges() public {
        (PriceOracle.Quote memory lo, bytes memory los) = _quote(ID, BUY, 50_000);
        vm.prank(creator);
        factory.launch(ID, "a", "A", "", 0, lo, los);
        (lo, los) = _quote(ID, BUY, 49_999);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(LaunchFactory.PriceOutOfBand.selector, uint256(49_999)));
        factory.launch(ID, "a", "A", "", 0, lo, los);
        (PriceOracle.Quote memory hi, bytes memory his) = _quote(ID, BUY, 950_000);
        vm.prank(creator);
        factory.launch(ID, "a", "A", "", 0, hi, his);
        (hi, his) = _quote(ID, BUY, 950_001);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(LaunchFactory.PriceOutOfBand.selector, uint256(950_001)));
        factory.launch(ID, "a", "A", "", 0, hi, his);
        // A per-market ceiling lifts it for that market only.
        vm.prank(owner);
        exchange.setMaxPrice(ID, 990_000);
        vm.prank(creator);
        factory.launch(ID, "a", "A", "", 0, hi, his);
        (hi, his) = _quote(ID_B, BUY, 950_001);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(LaunchFactory.PriceOutOfBand.selector, uint256(950_001)));
        factory.launch(ID_B, "a", "A", "", 0, hi, his);
    }

    function test_launchNeedsABuyQuoteForThatMarket() public {
        (PriceOracle.Quote memory s, bytes memory ss) = signedQuote(ID, SELL);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.WrongSide.selector, SELL));
        factory.launch(ID, "a", "A", "", 0, s, ss);
        (PriceOracle.Quote memory b, bytes memory bs) = signedQuote(ID_B, BUY);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(LaunchFactory.QuoteForOtherMarket.selector, ID_B, ID));
        factory.launch(ID, "a", "A", "", 0, b, bs);
        vm.warp(b.validUntil + 1);
        vm.prank(creator);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteExpired.selector, b.validUntil));
        factory.launch(ID_B, "a", "A", "", 0, b, bs);
    }

    function test_launchNeedsGraduatorAndFeeVault() public {
        LaunchFactory bare = new LaunchFactory(owner, exchange, oracle, platform);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.expectRevert(LaunchFactory.NoGraduator.selector);
        bare.launch(ID, "a", "A", "", 0, q, sig);
        vm.prank(owner);
        bare.setGraduator(graduator);
        vm.expectRevert(LaunchFactory.NoGraduator.selector);
        bare.launch(ID, "a", "A", "", 0, q, sig);
    }

    /* ---------------------------------------------------- who and how often */

    function test_manyCoinsOneMarketShareOnePToken() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        address first;
        for (uint256 i; i < 5; ++i) {
            vm.prank(address(uint160(0x1000 + i)));
            (, BondingCurve c) = factory.launch(ID, "Same", "SAME", "", uint16(i * 2_000), q, sig);
            if (i == 0) first = address(c.pToken());
            assertEq(address(c.pToken()), first);
            assertTrue(factory.isCurve(address(c)));
            assertEq(c.creator(), address(uint160(0x1000 + i)));
        }
        assertEq(factory.curveCount(), 5);
        assertEq(first, exchange.pTokenAddress(ID));
    }

    /// Documented: a launch quote is not consumed, so one quote can launch many
    /// coins in its window; and names are not unique, so a watcher can copy a
    /// pending launch's name and symbol and land first. The UI must identify coins
    /// by address, never by symbol.
    function test_quoteReplayAndNameCopyingAreAllowed() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(bob); // front-runner
        (Coin copy,) = factory.launch(ID, "No Hike", "NOHIKE", "ipfs://x", 0, q, sig);
        vm.prank(creator);
        (Coin orig,) = factory.launch(ID, "No Hike", "NOHIKE", "ipfs://x", 0, q, sig);
        assertTrue(address(copy) != address(orig));
        assertEq(copy.symbol(), orig.symbol());
    }

    function test_aContractCanLaunchAndIsTheCreator() public {
        LaunchBot bot = new LaunchBot();
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        (Coin c, BondingCurve cv) = bot.launch(factory, ID, q, sig);
        assertEq(cv.creator(), address(bot));
        (,,, address payee,) = vault.launches(address(c));
        assertEq(payee, address(bot));
    }

    /* ------------------------------------------------------- names and shape */

    function test_nameAndMetadataExtremes() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        bytes memory longName = new bytes(2_000);
        for (uint256 i; i < longName.length; ++i) {
            longName[i] = "a";
        }
        vm.prank(creator);
        (Coin empty,) = factory.launch(ID, "", "", "", 0, q, sig);
        assertEq(empty.name(), "");
        vm.prank(creator);
        (Coin long_,) = factory.launch(ID, string(longName), unicode"🚀🚀", string(longName), 0, q, sig);
        assertEq(bytes(long_.name()).length, 2_000);
        assertEq(long_.symbol(), unicode"🚀🚀");
        assertEq(long_.totalSupply(), long_.SUPPLY());
    }

    function test_holderBookOnlyWhenHoldersShare() public {
        (Coin none,) = _launch(ID, 0);
        (Coin some,) = _launch(ID, 1);
        (Coin all,) = _launch(ID, 10_000);
        assertEq(none.holderBook(), address(0));
        assertEq(some.holderBook(), address(vault));
        assertEq(all.holderBook(), address(vault));
    }

    function test_phantomAcrossTheBand() public {
        (PriceOracle.Quote memory lo, bytes memory los) = _quote(ID, BUY, 50_000);
        vm.prank(creator);
        (, BondingCurve cheap) = factory.launch(ID, "a", "A", "", 0, lo, los);
        // 6000 USDG x 2 / (5c x 5) = 48,000 shares.
        assertEq(cheap.phantom(), 48_000e6);
        (PriceOracle.Quote memory hi, bytes memory his) = _quote(ID, BUY, 950_000);
        vm.prank(creator);
        (, BondingCurve dear) = factory.launch(ID, "a", "A", "", 0, hi, his);
        assertEq(dear.phantom(), uint256(6_000e6 * 2e6) / (950_000 * 5));
        // Either way the raise at sell-out is ~$6k of shares at launch price.
        assertApproxEqRel(cheap.target() * 50_000 / 1e6, 6_000e6, 0.001e18);
        assertApproxEqRel(dear.target() * 950_000 / 1e6, 6_000e6, 0.001e18);
    }

    function test_launchedEventCarriesEverything() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.recordLogs();
        vm.prank(creator);
        (Coin c, BondingCurve cv) = factory.launch(ID, "No Hike", "NOHIKE", "ipfs://x", 2_500, q, sig);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool found;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != address(factory) || logs[i].topics[0] != LAUNCHED) continue;
            found = true;
            assertEq(uint256(logs[i].topics[1]), ID);
            assertEq(address(uint160(uint256(logs[i].topics[2]))), creator);
            (address coin_, address curve_, address p_, uint256 price, uint256 phantom, uint16 hb) =
                abi.decode(logs[i].data, (address, address, address, uint256, uint256, uint16));
            assertEq(coin_, address(c));
            assertEq(curve_, address(cv));
            assertEq(p_, address(cv.pToken()));
            assertEq(price, 600_000);
            assertEq(phantom, cv.phantom());
            assertEq(hb, 2_500);
        }
        assertTrue(found);
    }

    /* -------------------------------------------------------- owner settings */

    /// Regression: a zero platform would make every new curve's first trade
    /// revert (the platform fee goes to address(0)), and a graduation size out of
    /// range makes a curve that cannot trade (0 divides by zero) or never fills.
    function test_setConfigRejectsZeroPlatformAndOutOfRangeGraduationSize() public {
        vm.startPrank(owner);
        vm.expectRevert(LaunchFactory.BadParams.selector);
        factory.setConfig(address(0), 6_000e6);
        vm.expectRevert(LaunchFactory.BadParams.selector);
        factory.setConfig(platform, 0);
        vm.expectRevert(LaunchFactory.BadParams.selector);
        factory.setConfig(platform, 1_000e6 - 1);
        vm.expectRevert(LaunchFactory.BadParams.selector);
        factory.setConfig(platform, 1_000_000e6 + 1);
        // The bounds themselves are accepted.
        factory.setConfig(platform, 1_000e6);
        assertEq(factory.gradUsd(), 1_000e6);
        factory.setConfig(platform, 1_000_000e6);
        assertEq(factory.gradUsd(), 1_000_000e6);
        vm.stopPrank();
    }

    /// Only the owner may change the config.
    function test_setConfigIsOwnerOnly() public {
        vm.prank(alice);
        vm.expectRevert();
        factory.setConfig(alice, 6_000e6);
    }

    /* ---------------------------------------------------------- deploy script */

    PriceOracle internal dO;
    PExchange internal dEx;
    LaunchFactory internal dF;
    Router internal dRt;
    Graduator internal dG;
    FeeVault internal dV;
    address internal dOwner;
    address internal dSigner = makeAddr("signer");
    address internal dKeeper = makeAddr("keeperKey");
    address internal dBridge = makeAddr("bridgeDeposit");

    function _runDeploy() internal {
        vm.setEnv("PRICE_SIGNER", vm.toString(dSigner));
        vm.setEnv("KEEPER", vm.toString(dKeeper));
        vm.setEnv("USDG", vm.toString(address(usdg)));
        vm.setEnv("BRIDGE_DEPOSIT", vm.toString(dBridge));
        vm.setEnv("PLATFORM", vm.toString(address(0)));
        vm.setEnv("DEPLOY_OUT", "deployments/.harness-ac.json");
        vm.setEnv("DEV_USDG", "false");
        Deploy d = new Deploy();
        // A script's run() is called by its broadcaster, which is also tx.origin
        // here: call it through a forwarder placed at forge's default sender.
        address sender = 0x1804c8AB1F12E6bbf3894d4083f33e07309d1f38;
        vm.etch(sender, address(new RunForwarder()).code);
        RunForwarder(sender).run(d);
        string memory json = vm.readFile("deployments/.harness-ac.json");
        vm.removeFile("deployments/.harness-ac.json");
        dO = PriceOracle(vm.parseJsonAddress(json, ".oracle"));
        dEx = PExchange(vm.parseJsonAddress(json, ".exchange"));
        dF = LaunchFactory(vm.parseJsonAddress(json, ".factory"));
        dRt = Router(vm.parseJsonAddress(json, ".router"));
        dG = Graduator(vm.parseJsonAddress(json, ".graduator"));
        dV = FeeVault(vm.parseJsonAddress(json, ".feeVault"));
        dOwner = vm.parseJsonAddress(json, ".owner");
    }

    function test_deployScriptWiresEverything() public {
        _runDeploy();
        assertEq(dOwner, 0x1804c8AB1F12E6bbf3894d4083f33e07309d1f38);
        // Ownership: the broadcaster owns every owned contract.
        assertEq(dO.owner(), dOwner);
        assertEq(dEx.owner(), dOwner);
        assertEq(dF.owner(), dOwner);
        // Roles.
        assertEq(dO.signer(), dSigner);
        assertEq(dO.keeper(), dKeeper);
        assertEq(dO.poster(), dKeeper);
        assertEq(dEx.keeper(), dKeeper);
        assertEq(dEx.factory(), address(dF));
        assertEq(dEx.bridgeDeposit(), dBridge);
        assertEq(address(dEx.usdg()), address(usdg));
        assertEq(address(dEx.oracle()), address(dO));
        _checkFactory();
        _checkPeriphery();
    }

    function _checkFactory() internal view {
        assertEq(address(dF.exchange()), address(dEx));
        assertEq(address(dF.oracle()), address(dO));
        assertEq(address(dF.graduator()), address(dG));
        assertEq(address(dF.feeVault()), address(dV));
        assertEq(dF.platform(), address(dEx), "platform defaults to the exchange");
        assertEq(dF.gradUsd(), 6_000e6);
        (uint16 cf, uint16 cs, uint24 pf, uint16 ps) = dF.fees();
        assertEq(cf, 140);
        assertEq(cs, 5_000);
        assertEq(pf, 10_000);
        assertEq(ps, 5_000);
    }

    function _checkPeriphery() internal view {
        // Graduator: the hook flags and its factory.
        assertEq(uint160(address(dG)) & 0x3FFF, 0x2000);
        assertEq(dG.factory(), address(dF));
        assertEq(address(dG.poolManager()), address(poolManager));
        assertEq(dV.factory(), address(dF));
        assertEq(address(dV.exchange()), address(dEx));
        assertEq(dV.poolManager(), address(poolManager));
        assertEq(address(dRt.exchange()), address(dEx));
        assertEq(address(dRt.usdg()), address(usdg));
        assertEq(address(dRt.poolManager()), address(poolManager));
        // Limits.
        assertEq(dEx.outflowCapPerHour(), 1_000_000e6);
        assertEq(dEx.postedSpreadBps(), 150);
        // The posted path ships closed.
        assertEq(dEx.postedMaxTrade(), 0);
        assertEq(dEx.postedMaxPerBlock(), 0);
        assertEq(dO.maxPostAge(), 900);
        assertFalse(dEx.halted());
    }

    /// End to end on the scripted deployment: launch, buy, sell out, graduate.
    function test_deployedSystemGraduatesACoin() public {
        _runDeploy();
        uint256 pk = 0x5151;
        vm.setEnv("PRICE_SIGNER", "");
        address s = vm.addr(pk);
        // Swap in a signer we hold the key for (two-day wait).
        vm.prank(dOwner);
        dO.setSigner(s);
        vm.warp(block.timestamp + 2 days);
        vm.prank(dOwner);
        dO.acceptSigner();
        PriceOracle.Quote memory q = PriceOracle.Quote(ID, BUY, 600_000, type(uint256).max, uint64(block.timestamp + 10));
        bytes32 digest = dO.quoteDigest(q);
        (uint8 v, bytes32 r, bytes32 ss) = vm.sign(pk, digest);
        bytes memory sig = abi.encodePacked(r, ss, v);
        vm.prank(creator);
        (, BondingCurve cv) = dF.launch(ID, "a", "A", "", 5_000, q, sig);
        vm.prank(dOwner);
        dEx.setMaxUnbacked(ID, type(uint256).max);
        vm.startPrank(alice);
        usdg.approve(address(dRt), type(uint256).max);
        dRt.buy(cv, 7_000e6, 0, alice, q, sig);
        vm.stopPrank();
        assertTrue(cv.graduated());
    }
}
