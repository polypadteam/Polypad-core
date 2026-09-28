// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {BondingCurve} from "../src/BondingCurve.sol";
import {Coin} from "../src/Coin.sol";
import {FeeVault} from "../src/FeeVault.sol";
import {Graduator} from "../src/Graduator.sol";
import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";
import {LaunchFactory} from "../src/LaunchFactory.sol";
import {PExchange} from "../src/PExchange.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {PToken} from "../src/PToken.sol";
import {Router} from "../src/Router.sol";

contract MockUSDG is ERC20 {
    constructor() ERC20("Global Dollar", "USDG") {}

    function decimals() public pure override returns (uint8) {
        return 6;
    }

    /// @dev Like the real USDG, which can freeze an address.
    mapping(address => bool) public frozen;

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function freeze(address who) external {
        frozen[who] = true;
    }

    function unfreeze(address who) external {
        frozen[who] = false;
    }

    function _update(address from, address to, uint256 value) internal override {
        require(!frozen[from] && !frozen[to], "frozen");
        super._update(from, to, value);
    }
}

/// @dev Shared deployment: one market at 60c, the Fed example from the design doc.
contract PolypadBase is Test {
    uint256 internal constant ID = 0xFED;
    uint256 internal constant ID_B = 0xB0B;
    uint8 internal constant BUY = 0;
    uint8 internal constant SELL = 1;

    uint256 internal signerPk = 0xA11CE;
    address internal signer = vm.addr(0xA11CE);
    address internal owner = makeAddr("owner");
    address internal keeper = makeAddr("keeper");
    address internal platform = makeAddr("platform");
    address internal bridge = makeAddr("bridge");
    address internal creator = makeAddr("creator");
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");

    MockUSDG internal usdg;
    PriceOracle internal oracle;
    PExchange internal exchange;
    LaunchFactory internal factory;
    Router internal router;
    Graduator internal graduator;
    FeeVault internal vault;
    /// @dev Uniswap's PoolManager, its Robinhood Chain bytecode at its Robinhood Chain address.
    IPoolManager internal poolManager = IPoolManager(0x8366a39CC670B4001A1121B8F6A443A643e40951);

    /// @dev What the pricer would quote right now, per market (both sides, for simplicity).
    mapping(uint256 => uint64) internal px;

    function setUp() public virtual {
        vm.warp(1_800_000_000);
        usdg = new MockUSDG();
        oracle = new PriceOracle(owner, signer, keeper);
        exchange = new PExchange(owner, IERC20(address(usdg)), oracle, keeper);
        factory = new LaunchFactory(owner, exchange, oracle, platform);
        vm.etch(address(poolManager), vm.parseBytes(vm.readFile("test/fixtures/PoolManager.hex")));
        // Any address whose low 14 bits are exactly BEFORE_INITIALIZE.
        address hookAt = address(uint160(0x1234560000000000000000000000000000000000) | 0x2000);
        deployCodeTo("Graduator.sol:Graduator", abi.encode(poolManager, address(factory)), hookAt);
        graduator = Graduator(hookAt);
        router = new Router(IERC20(address(usdg)), exchange, poolManager);
        vault = new FeeVault(address(factory), exchange, address(poolManager));
        vm.startPrank(owner);
        exchange.setRoles(keeper, address(factory), bridge);
        factory.setGraduator(graduator);
        factory.setFeeVault(vault);
        vm.stopPrank();

        // A deep float so redemptions never fail for lack of USDG in these tests.
        usdg.mint(address(exchange), 100_000e6);
        usdg.mint(alice, 100_000e6);
        usdg.mint(bob, 100_000e6);
        vm.prank(alice);
        usdg.approve(address(router), type(uint256).max);
        vm.prank(bob);
        usdg.approve(address(router), type(uint256).max);

        _post(ID, 600_000);
        _post(ID_B, 300_000);
    }

    function _post(uint256 id, uint64 price) internal {
        px[id] = price;
    }

    /// @dev Sign a quote as the pricer would. Makes no external calls, so it is safe
    ///      between `vm.expectRevert` and the call under test.
    function _sign(PriceOracle.Quote memory q, uint256 pk) internal view returns (bytes memory) {
        bytes32 domain = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("Polypad Oracle"),
                keccak256("1"),
                block.chainid,
                address(oracle)
            )
        );
        bytes32 structHash = keccak256(
            abi.encode(
                keccak256("Quote(uint256 positionId,uint8 side,uint64 price,uint256 maxAmount,uint64 validUntil)"),
                q.positionId,
                q.side,
                q.price,
                q.maxAmount,
                q.validUntil
            )
        );
        (uint8 v, bytes32 r, bytes32 sv) = vm.sign(pk, keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, sv, v);
    }

    function signedQuote(uint256 id, uint8 side) public view returns (PriceOracle.Quote memory q, bytes memory sig) {
        q = PriceOracle.Quote(id, side, px[id], type(uint256).max, uint64(block.timestamp + 15));
        sig = _sign(q, signerPk);
    }

    function _launch(uint256 id) internal returns (Coin coin, BondingCurve curve) {
        return _launch(id, 0);
    }

    function _launch(uint256 id, uint16 holdersBps) internal returns (Coin coin, BondingCurve curve) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(id, BUY);
        vm.prank(creator);
        (coin, curve) = factory.launch(id, "No Hike", "NOHIKE", "ipfs://x", holdersBps, q, sig);
    }

    function _buy(address who, BondingCurve c, uint256 usdgIn) internal returns (uint256 coins, uint256 refund) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(who);
        return router.buy(c, usdgIn, 0, who, q, sig);
    }

    function _sell(address who, BondingCurve c, uint256 coins) internal returns (uint256) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.startPrank(who);
        c.coin().approve(address(router), coins);
        uint256 out = router.sell(c, coins, 0, who, q, sig);
        vm.stopPrank();
        return out;
    }

    function _back(uint256 id, uint256 amount) internal {
        uint256[] memory ids = new uint256[](1);
        uint256[] memory amounts = new uint256[](1);
        ids[0] = id;
        amounts[0] = amount;
        vm.prank(keeper);
        exchange.reportBacked(ids, amounts);
    }

    function _pause(uint256 id) internal {
        uint256[] memory ids = new uint256[](1);
        ids[0] = id;
        vm.prank(keeper);
        oracle.setPaused(ids, true);
    }
}

contract LaunchTest is PolypadBase {
    function test_launchCreatesCoinCurveAndPToken() public {
        (Coin coin, BondingCurve curve) = _launch(ID);
        PToken p = exchange.pTokenOf(ID);
        assertEq(address(curve.pToken()), address(p));
        assertEq(address(curve.coin()), address(coin));
        assertEq(coin.balanceOf(address(curve)), coin.SUPPLY());
        assertEq(curve.reserved(), coin.SUPPLY() * 2 / 7);
        assertEq(exchange.pTokenAddress(ID), address(p));
        // $6k at 60c is 10,000 shares; phantom is 0.4x that.
        assertApproxEqAbs(curve.phantom(), 4_000e6, 1);
        assertApproxEqAbs(curve.target(), 10_000e6, 3);
    }

    function test_secondCoinReusesPToken() public {
        (, BondingCurve a) = _launch(ID);
        (, BondingCurve b) = _launch(ID);
        assertEq(address(a.pToken()), address(b.pToken()));
        assertEq(factory.curveCount(), 2);
    }

    function test_launchRefusesExpiredPausedOrOutOfBand() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.warp(block.timestamp + 16);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteExpired.selector, q.validUntil));
        factory.launch(ID, "No Hike", "NOHIKE", "ipfs://x", 0, q, sig);

        _post(ID, 985_000);
        vm.expectRevert(abi.encodeWithSelector(LaunchFactory.PriceOutOfBand.selector, 985_000));
        _launch(ID);

        _post(ID, 600_000);
        _pause(ID);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        _launch(ID);
    }

    function test_launchRefusesQuoteForAnotherMarket() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID_B, BUY);
        vm.expectRevert(abi.encodeWithSelector(LaunchFactory.QuoteForOtherMarket.selector, ID_B, ID));
        factory.launch(ID, "No Hike", "NOHIKE", "ipfs://x", 0, q, sig);
    }
}

contract TradeTest is PolypadBase {
    Coin internal coin;
    BondingCurve internal curve;
    PToken internal p;

    function setUp() public override {
        super.setUp();
        (coin, curve) = _launch(ID);
        p = exchange.pTokenOf(ID);
        // Most tests here are about trading, not backing; the cap has its own test.
        vm.prank(owner);
        exchange.setMaxRisk(ID, type(uint256).max);
    }

    function test_buyMintsSharesAtQuotePlusSpread() public {
        (uint256 coins,) = _buy(alice, curve, 600e6);
        assertGt(coins, 0);
        assertEq(coin.balanceOf(alice), coins);
        // 600 USDG at 60c + 0.25% buys 997.5 shares (rounded down).
        uint256 buyPrice = (uint256(600_000) * 10_025 + 9_999) / 10_000;
        uint256 expectedShares = (uint256(600e6) * 1e6) / buyPrice;
        assertEq(p.totalSupply(), expectedShares);
        // Curve holds the shares net of the 1.4% fee (rounded up); the fee went half to the creator side, half to the platform.
        uint256 fee = (expectedShares * 140 + 9_999) / 10_000;
        assertEq(curve.trackedQuote(), expectedShares - fee);
        assertEq(vault.owed(address(p), creator), fee * 5_000 / 10_000);
        assertEq(p.balanceOf(platform), fee - fee * 5_000 / 10_000);
    }

    function test_sellReturnsUsdgAndBurnsShares() public {
        (uint256 coins,) = _buy(alice, curve, 600e6);
        uint256 before = usdg.balanceOf(alice);
        uint256 out = _sell(alice, curve, coins);
        assertEq(usdg.balanceOf(alice), before + out);
        // Round trip costs two 1.4% curve fees and two 0.25% spreads, about 3.3%.
        assertApproxEqRel(out, 600e6 * 967 / 1000, 0.005e18);
        assertEq(coin.balanceOf(alice), 0);
        // Only the fee shares remain in circulation.
        assertEq(
            p.totalSupply(),
            p.balanceOf(address(vault)) + p.balanceOf(platform) + p.balanceOf(address(exchange)) + curve.trackedQuote()
        );
    }

    /// @dev Buying at the ask and selling at the bid: the user pays the book's gap, not the float.
    function test_buyAndSellUseTheirOwnSide() public {
        PriceOracle.Quote memory bq =
            PriceOracle.Quote(ID, BUY, 610_000, type(uint256).max, uint64(block.timestamp + 15));
        PriceOracle.Quote memory sq =
            PriceOracle.Quote(ID, SELL, 590_000, type(uint256).max, uint64(block.timestamp + 15));
        vm.prank(alice);
        router.buy(curve, 610e6, 0, alice, bq, _sign(bq, signerPk));
        // 610 USDG at 61c + spread: just under 1,000 shares.
        assertApproxEqRel(p.totalSupply(), 997.5e6, 0.001e18);

        uint256 coins = coin.balanceOf(alice);
        vm.startPrank(alice);
        coin.approve(address(router), coins);
        uint256 out = router.sell(curve, coins, 0, alice, sq, _sign(sq, signerPk));
        vm.stopPrank();
        // Back at 59c less the spread and two curve fees.
        assertApproxEqRel(out, 610e6 * 59 / 61 * 967 / 1000, 0.005e18);
    }

    /// @dev The design doc's step 3: the odds move and the coin's dollar value follows with no trade.
    function test_coinDollarValueFollowsOdds() public {
        _buy(alice, curve, 3_000e6);
        uint256 spotShares = curve.spotPrice();
        uint256 mcapAt60 = spotShares * 600_000 * 1_000_000_000 / 1e6;
        _post(ID, 900_000);
        uint256 mcapAt90 = curve.spotPrice() * px[ID] * 1_000_000_000 / 1e6;
        assertEq(curve.spotPrice(), spotShares);
        assertEq(mcapAt90 * 2, mcapAt60 * 3);
    }

    function test_mintRefusesExpiredQuote() public {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.warp(block.timestamp + 16);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteExpired.selector, q.validUntil));
        router.buy(curve, 100e6, 0, alice, q, sig);
    }

    function test_mintRefusesQuoteValidTooLong() public {
        PriceOracle.Quote memory q =
            PriceOracle.Quote(ID, BUY, 600_000, type(uint256).max, uint64(block.timestamp + 121));
        bytes memory sig = _sign(q, signerPk);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.QuoteTooLong.selector, q.validUntil));
        router.buy(curve, 100e6, 0, alice, q, sig);
    }

    function test_mintRefusesForgedOrAlteredQuote() public {
        PriceOracle.Quote memory q =
            PriceOracle.Quote(ID, BUY, 600_000, type(uint256).max, uint64(block.timestamp + 15));
        bytes memory forged = _sign(q, 0xBAD);
        vm.prank(alice);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        router.buy(curve, 100e6, 0, alice, q, forged);

        bytes memory sig = _sign(q, signerPk);
        q.price = 100_000; // cheaper than signed
        vm.prank(alice);
        vm.expectRevert(PriceOracle.BadSignature.selector);
        router.buy(curve, 100e6, 0, alice, q, sig);
    }

    function test_quotesAreSidedSizedAndPerMarket() public {
        (PriceOracle.Quote memory sq, bytes memory ssig) = signedQuote(ID, SELL);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.WrongSide.selector, SELL));
        router.buy(curve, 100e6, 0, alice, sq, ssig);

        PriceOracle.Quote memory small = PriceOracle.Quote(ID, BUY, 600_000, 50e6, uint64(block.timestamp + 15));
        bytes memory smallSig = _sign(small, signerPk);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteTooSmall.selector, 100e6, 50e6));
        router.buy(curve, 100e6, 0, alice, small, smallSig);

        (PriceOracle.Quote memory other, bytes memory otherSig) = signedQuote(ID_B, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.QuoteForOtherMarket.selector, ID_B, ID));
        router.buy(curve, 100e6, 0, alice, other, otherSig);
    }

    function test_pausedMarketBlocksBuysNotSells() public {
        (uint256 coins,) = _buy(alice, curve, 100e6);
        _pause(ID);

        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(bob);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.MarketPaused.selector, ID));
        router.buy(curve, 100e6, 0, bob, q, sig);

        _sell(alice, curve, coins);
    }

    function test_mintRefusesOutsideBand() public {
        _post(ID, 985_000);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(PExchange.PriceOutOfBand.selector, 985_000));
        router.buy(curve, 100e6, 0, alice, q, sig);
    }

    function test_riskCapStopsMints() public {
        // At 60c each unbacked share risks 40c: a $200 cap is 500 shares.
        vm.prank(owner);
        exchange.setMaxRisk(ID, 200e6);
        _buy(alice, curve, 290e6); // ~482 shares, ~$193 at risk
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(alice);
        vm.expectPartialRevert(PExchange.RiskCap.selector);
        router.buy(curve, 100e6, 0, alice, q, sig); // ~648 shares, ~$259 at risk

        _back(ID, 482e6);
        _buy(alice, curve, 100e6);
    }

    /// The cap is in dollars at risk, (1 - price) a share: near-certain shares
    /// can go unhedged in size, long shots cannot.
    function test_riskCapScalesWithPrice() public {
        assertEq(exchange.defaultMaxRisk(), 1_000e6);
        vm.prank(alice);
        usdg.approve(address(exchange), type(uint256).max);

        // 97c: ~10,283 unbacked shares risk ~$308, under $1,000.
        vm.prank(owner);
        exchange.setMaxPrice(ID_B, 990_000);
        vm.prank(owner);
        exchange.ensurePToken(ID_B);
        address pb = address(exchange.pTokenOf(ID_B));
        _post(ID_B, 970_000);
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID_B, BUY);
        vm.prank(alice);
        uint256 got = exchange.mint(pb, 10_000e6, 0, alice, q, sig);
        assertGt(got, 10_000e6);

        // 5c: $60 buys ~1,197 shares risking ~$1,137, over $1,000.
        vm.prank(owner);
        exchange.setMaxRisk(ID_B, 0); // back to the default
        _post(ID_B, 50_000);
        _back(ID_B, got);
        (q, sig) = signedQuote(ID_B, BUY);
        vm.prank(alice);
        vm.expectPartialRevert(PExchange.RiskCap.selector);
        exchange.mint(pb, 60e6, 0, alice, q, sig);
        vm.prank(alice);
        exchange.mint(pb, 40e6, 0, alice, q, sig); // ~797 shares, ~$757
    }

    function test_settlementPaysPayoutLessSettleFee() public {
        (uint256 coins,) = _buy(alice, curve, 600e6);
        vm.prank(keeper);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());

        // Settled redemptions need no quote and pay $1 per share less 0.5%.
        vm.warp(block.timestamp + 1 days);
        PriceOracle.Quote memory none;
        vm.startPrank(alice);
        coin.approve(address(router), coins);
        uint256 out = router.sell(curve, coins, 0, alice, none, "");
        vm.stopPrank();
        // ~997 shares bought at 60c, curve returns ~98% after its fees, paid at $1.
        assertGt(out, 950e6);
    }

    function test_settledNoPaysZeroAndRefusesMints() public {
        (uint256 coins,) = _buy(alice, curve, 600e6);
        vm.prank(keeper);
        oracle.settle(ID, 0);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        assertEq(_sell(alice, curve, coins), 0);
        PriceOracle.Quote memory none;
        vm.prank(bob);
        vm.expectRevert(abi.encodeWithSelector(PExchange.SettledAtZero.selector, ID));
        router.buy(curve, 100e6, 0, bob, none, "");
    }

    function test_absorbBurnsPTokenSentToTheExchange() public {
        _buy(alice, curve, 600e6);
        vm.startPrank(creator);
        vault.withdraw(address(p), creator);
        p.transfer(address(exchange), 1e6);
        vm.stopPrank();
        uint256 held = p.balanceOf(address(exchange));
        uint256 supply = p.totalSupply();
        assertGt(held, 0);
        exchange.absorb(address(p));
        assertEq(p.totalSupply(), supply - held);
        assertEq(p.balanceOf(address(exchange)), 0);
    }

    function test_sellOutGraduatesAndSpendsTheRestInThePool() public {
        // Target is ~10,000 shares (~$6k); pay far more.
        (uint256 coins, uint256 refund) = _buy(alice, curve, 20_000e6);
        assertTrue(curve.graduated());
        assertEq(refund, 0);
        assertEq(p.balanceOf(alice), 0);
        // The curve's sellable supply, plus more from the pool.
        assertGt(coins, coin.SUPPLY() - curve.reserved());
        assertEq(coin.balanceOf(alice), coins);
    }

    function test_onlyRolesCanActOnOracleAndExchange() public {
        uint256[] memory ids = new uint256[](1);
        ids[0] = ID;
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.setPaused(ids, true);
        vm.expectRevert(PriceOracle.OnlyKeeper.selector);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.expectRevert(PExchange.OnlyKeeper.selector);
        exchange.sendToBridge(1);
        vm.expectRevert(PExchange.OnlyFactory.selector);
        exchange.ensurePToken(123);
        vm.expectRevert(PToken.OnlyExchange.selector);
        p.mint(alice, 1);
    }

    function test_settlementIsOneWay() public {
        vm.prank(keeper);
        oracle.settle(ID, 1e6);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
        vm.prank(keeper);
        vm.expectRevert(abi.encodeWithSelector(PriceOracle.AlreadySettled.selector, ID));
        oracle.settle(ID, 0);
        vm.warp(block.timestamp + oracle.SETTLE_DELAY());
    }

    function test_directTransfersDoNotMovePrice() public {
        (uint256 coins,) = _buy(alice, curve, 600e6);
        uint256 spot = curve.spotPrice();
        vm.prank(alice);
        coin.transfer(address(curve), coins / 2);
        assertEq(curve.spotPrice(), spot);
    }
}
