// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";

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

/// @dev The parts of Paxos's USDG (a facet proxy) the fork tests drive.
interface IUSDG {
    function freeze(address) external;
    function isFrozen(address) external view returns (bool);
    function pause() external;
    function unpause() external;
    function paused() external view returns (bool);
    function grantRole(bytes32, address) external;
    function defaultAdmin() external view returns (address);
}

/**
 * @dev v7 deployed on a fork of Robinhood Chain exactly as script/Deploy.s.sol
 *      does it: real USDG, the live Uniswap v4 PoolManager, the Graduator at a
 *      CREATE2 address mined through the canonical deployer.
 *
 *      Needs RH_FORK_URL (an archive-capable RPC); every test skips without it.
 *      FORK_BLOCK pins the block (default: a recent one, so reruns hit the cache).
 */
abstract contract ForkBase is Test {
    address internal constant USDG = 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168;
    address internal constant POOL_MANAGER = 0x8366a39CC670B4001A1121B8F6A443A643e40951;
    address internal constant CREATE2_DEPLOYER = 0x4e59b44847b379578588920cA78FbF26c0B4956C;
    uint256 internal constant ID = 0xFED;
    uint256 internal constant ID_B = 0xB0B;
    uint8 internal constant BUY = 0;
    uint8 internal constant SELL = 1;

    uint256 internal signerPk = 0xA11CE;
    address internal signer = vm.addr(0xA11CE);
    address internal owner = makeAddr("owner");
    address internal keeper = makeAddr("keeper");
    address internal bridge = makeAddr("bridge");
    address internal creator = makeAddr("creator");
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");

    IERC20 internal usdg = IERC20(USDG);
    IPoolManager internal poolManager = IPoolManager(POOL_MANAGER);
    PriceOracle internal oracle;
    PExchange internal exchange;
    LaunchFactory internal factory;
    Router internal router;
    Graduator internal graduator;
    FeeVault internal vault;
    address internal platform;

    mapping(uint256 => uint64) internal px;

    function setUp() public virtual {
        if (!vm.envExists("RH_FORK_URL")) {
            vm.skip(true);
            return;
        }
        vm.createSelectFork(vm.envString("RH_FORK_URL"), vm.envOr("FORK_BLOCK", uint256(74_043_800)));
        _deploy();
        px[ID] = 600_000;
        px[ID_B] = 300_000;

        _fund(address(exchange), 100_000e6);
        address[3] memory who = [alice, bob, carol];
        for (uint256 i; i < who.length; ++i) {
            _fund(who[i], 100_000e6);
            vm.prank(who[i]);
            usdg.approve(address(router), type(uint256).max);
        }
    }

    /// @dev Deploy.s.sol's `run`, with `owner` as the broadcaster.
    function _deploy() internal {
        vm.startPrank(owner);
        oracle = new PriceOracle(owner, signer, keeper);
        exchange = new PExchange(owner, usdg, oracle, keeper);
        platform = address(exchange); // Deploy.s.sol's default
        factory = new LaunchFactory(owner, exchange, oracle, platform);
        vm.stopPrank();

        // The Graduator goes through the CREATE2 deployer, as a forge broadcast does.
        assertGt(CREATE2_DEPLOYER.code.length, 0, "no CREATE2 deployer on chain");
        Deploy d = new Deploy();
        bytes memory init = abi.encodePacked(type(Graduator).creationCode, abi.encode(poolManager, address(factory)));
        (bytes32 salt, address expected) = d.mineSalt(keccak256(init));
        (bool ok, bytes memory ret) = CREATE2_DEPLOYER.call(abi.encodePacked(salt, init));
        require(ok && address(bytes20(ret)) == expected, "graduator create2");
        graduator = Graduator(expected);

        vm.startPrank(owner);
        factory.setGraduator(graduator);
        vault = new FeeVault(address(factory), exchange, address(poolManager));
        factory.setFeeVault(vault);
        router = new Router(usdg, exchange, poolManager);
        exchange.setRoles(keeper, address(factory), bridge);
        exchange.setOutflowCap(1_000_000e6, 5_000);
        vm.stopPrank();
    }

    /// @dev Real USDG balance via storage (Paxos keeps plain balances; `deal` finds the slot).
    function _fund(address who, uint256 amount) internal {
        deal(USDG, who, usdg.balanceOf(who) + amount);
    }

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
        bytes32 structHash = keccak256(abi.encode(oracle.QUOTE_TYPEHASH(), q.positionId, q.side, q.price, q.maxAmount, q.validUntil));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, s, v);
    }

    function signedQuote(uint256 id, uint8 side) internal view returns (PriceOracle.Quote memory q, bytes memory sig) {
        q = PriceOracle.Quote(id, side, px[id], type(uint256).max, uint64(block.timestamp + 15));
        sig = _sign(q, signerPk);
    }

    function _launch(uint256 id, uint16 holdersBps) internal returns (Coin coin, BondingCurve curve) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(id, BUY);
        vm.prank(creator);
        (coin, curve) = factory.launch(id, "No Hike", "NOHIKE", "ipfs://x", holdersBps, q, sig);
        vm.prank(owner);
        exchange.setMaxRisk(id, type(uint256).max);
    }

    function _buy(address who, BondingCurve c, uint256 usdgIn) internal returns (uint256 coins, uint256 refund) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, BUY);
        vm.prank(who);
        return router.buy(c, usdgIn, 0, who, q, sig);
    }

    function _sell(address who, BondingCurve c, uint256 coins) internal returns (uint256 out) {
        (PriceOracle.Quote memory q, bytes memory sig) = signedQuote(ID, SELL);
        vm.startPrank(who);
        c.coin().approve(address(router), coins);
        out = router.sell(c, coins, 0, who, q, sig);
        vm.stopPrank();
    }

    /// @dev Move the price the pricer quotes.
    function _postOnChain(uint256 id, uint64 price) internal {
        px[id] = price;
    }

    /// @dev USDG's asset-protection and pause roles, granted by its default admin.
    function _usdgRole(string memory role, address to) internal {
        IUSDG u = IUSDG(USDG);
        vm.prank(u.defaultAdmin());
        u.grantRole(keccak256(bytes(role)), to);
    }
}
