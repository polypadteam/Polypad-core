// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script, console2} from "forge-std/Script.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";

import {Graduator} from "../src/Graduator.sol";
import {LaunchFactory} from "../src/LaunchFactory.sol";
import {PExchange} from "../src/PExchange.sol";
import {PriceOracle} from "../src/PriceOracle.sol";
import {Router} from "../src/Router.sol";

/// @dev Local runs only: stands in for USDG on a chain without it.
contract DevUSDG is ERC20 {
    constructor(address to) ERC20("Dev Global Dollar", "USDG") {
        _mint(to, 1_000_000e6);
    }

    function decimals() public pure override returns (uint8) {
        return 6;
    }
}

/**
 * Deploys the phase 1 contracts and wires their roles.
 *
 * Env:
 *   PRICE_SIGNER     signs quotes off chain (holds no funds)
 *   KEEPER           reports backing, pauses and settles markets, sends float to the
 *                    bridge, may halt the exchange, and posts prices (poster)
 *   PLATFORM         receives the platform fee (default: the exchange, so fees build the float)
 *   USDG             collateral; omit with DEV_USDG=1 to deploy a dev token
 *   BRIDGE_DEPOSIT   the desk's Polymarket deposit address (optional; set later with setRoles)
 *   GRAD_USD         graduation target in USDG units (default 6000e6)
 *   POOL_MANAGER     Uniswap v4 PoolManager (default: Robinhood Chain's)
 *   OUTFLOW_CAP      hourly redemption cap in USDG units (default 1,000,000e6)
 *   POSTED_MAX_TRADE / POSTED_MAX_BLOCK  posted-path limits for terminals (default 5,000e6 / 20,000e6)
 *   DEPLOY_OUT       output path (default deployments/<chainid>.json; set it for local runs)
 *
 * The broadcaster becomes the owner of the oracle, exchange and factory.
 * The Graduator is the v4 hook of every Polypad pool, so it is deployed through
 * the CREATE2 deployer at a salt mined for an address whose low 14 bits are
 * exactly BEFORE_INITIALIZE (0x2000).
 * Writes the addresses to DEPLOY_OUT.
 *
 * Verify every contract on Sourcify (Blockscout shows Sourcify sources):
 *   forge script script/Deploy.s.sol --broadcast --verify --verifier sourcify ...
 */
contract Deploy is Script {
    address internal constant CREATE2_DEPLOYER = 0x4e59b44847b379578588920cA78FbF26c0B4956C;
    address internal constant RH_POOL_MANAGER = 0x8366a39CC670B4001A1121B8F6A443A643e40951;
    uint160 internal constant HOOK_MASK = 0x3FFF;
    uint160 internal constant HOOK_FLAGS = 0x2000; // BEFORE_INITIALIZE only

    /// @notice First salt whose CREATE2 address carries exactly the hook flags.
    function mineSalt(bytes32 initCodeHash) public pure returns (bytes32 salt, address at) {
        for (uint256 i; i < 1_000_000; ++i) {
            salt = bytes32(i);
            at = address(
                uint160(uint256(keccak256(abi.encodePacked(bytes1(0xff), CREATE2_DEPLOYER, salt, initCodeHash))))
            );
            if (uint160(at) & HOOK_MASK == HOOK_FLAGS) return (salt, at);
        }
        revert("no salt");
    }

    function _graduator(IPoolManager poolManager, address factory) internal returns (Graduator g) {
        (bytes32 salt, address expected) =
            mineSalt(keccak256(abi.encodePacked(type(Graduator).creationCode, abi.encode(poolManager, factory))));
        g = new Graduator{salt: salt}(poolManager, factory);
        require(address(g) == expected, "graduator address");
    }

    function run() external {
        address priceSigner = vm.envAddress("PRICE_SIGNER");
        address keeper = vm.envAddress("KEEPER");
        address platform = vm.envOr("PLATFORM", address(0));
        address bridgeDeposit = vm.envOr("BRIDGE_DEPOSIT", address(0));
        uint256 gradUsd = vm.envOr("GRAD_USD", uint256(6_000e6));
        IPoolManager poolManager = IPoolManager(vm.envOr("POOL_MANAGER", RH_POOL_MANAGER));
        uint256 outflowCap = vm.envOr("OUTFLOW_CAP", uint256(1_000_000e6));

        vm.startBroadcast();
        address owner = msg.sender;
        uint256 fromBlock = block.number;

        IERC20 usdg = vm.envOr("DEV_USDG", false) ? IERC20(address(new DevUSDG(owner))) : IERC20(vm.envAddress("USDG"));

        PriceOracle oracle = new PriceOracle(owner, priceSigner, keeper);
        PExchange exchange = new PExchange(owner, usdg, oracle, keeper);
        if (platform == address(0)) platform = address(exchange);
        LaunchFactory factory = new LaunchFactory(owner, exchange, oracle, platform);
        Graduator graduator = _graduator(poolManager, address(factory));
        factory.setGraduator(graduator);
        Router router = new Router(usdg, exchange, poolManager);
        exchange.setRoles(keeper, address(factory), bridgeDeposit);
        exchange.setOutflowCap(outflowCap);
        exchange.setPostedParams(150, vm.envOr("POSTED_MAX_TRADE", uint256(5_000e6)), vm.envOr("POSTED_MAX_BLOCK", uint256(20_000e6)));
        oracle.setPoster(keeper);
        if (gradUsd != 6_000e6) factory.setConfig(platform, gradUsd);
        vm.stopBroadcast();

        string memory key = "deployment";
        vm.serializeAddress(key, "usdg", address(usdg));
        vm.serializeAddress(key, "oracle", address(oracle));
        vm.serializeAddress(key, "exchange", address(exchange));
        vm.serializeAddress(key, "factory", address(factory));
        vm.serializeAddress(key, "router", address(router));
        vm.serializeAddress(key, "graduator", address(graduator));
        vm.serializeAddress(key, "poolManager", vm.envOr("POOL_MANAGER", RH_POOL_MANAGER));
        vm.serializeAddress(key, "owner", owner);
        string memory json = vm.serializeUint(key, "fromBlock", fromBlock);
        string memory path = vm.envOr("DEPLOY_OUT", string.concat("deployments/", vm.toString(block.chainid), ".json"));
        vm.writeJson(json, path);

        console2.log("oracle  ", address(oracle));
        console2.log("exchange", address(exchange));
        console2.log("factory ", address(factory));
        console2.log("router  ", address(router));
        console2.log("graduator", address(graduator));
        console2.log("usdg    ", address(usdg));
        console2.log("written ", path);
    }
}
