// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script, console2} from "forge-std/Script.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";

import {FeeVault} from "../src/FeeVault.sol";
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
 *                    bridge, and may halt the exchange
 *   PLATFORM         receives the platform fee (default: the exchange, so fees build the float)
 *   USDG             collateral; omit with DEV_USDG=1 to deploy a dev token
 *   BRIDGE_DEPOSIT   the desk's Polymarket deposit address (optional; set later with setRoles)
 *   GRAD_USD         graduation target in USDG units (default 6000e6)
 *   POOL_MANAGER     Uniswap v4 PoolManager (default: Robinhood Chain's)
 *   OUTFLOW_FLOOR    hourly redemption cap floor in USDG units (default 5,000e6)
 *   OUTFLOW_FLOAT_BPS  hourly redemption cap as bps of the float (default 5,000 = 50%, at most 10,000)
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

    function _wire(LaunchFactory factory, PExchange exchange, IPoolManager poolManager)
        internal
        returns (Graduator graduator, FeeVault feeVault)
    {
        graduator = _graduator(poolManager, address(factory));
        factory.setGraduator(graduator);
        feeVault = new FeeVault(address(factory), exchange, address(poolManager));
        factory.setFeeVault(feeVault);
    }

    struct Out {
        IERC20 usdg;
        PriceOracle oracle;
        PExchange exchange;
        LaunchFactory factory;
        Router router;
        Graduator graduator;
        FeeVault feeVault;
        address owner;
        uint256 fromBlock;
    }

    function run() external {
        address platform = vm.envOr("PLATFORM", address(0));
        IPoolManager poolManager = IPoolManager(vm.envOr("POOL_MANAGER", RH_POOL_MANAGER));

        vm.startBroadcast();
        Out memory o;
        o.owner = msg.sender;
        o.fromBlock = block.number;
        o.usdg = vm.envOr("DEV_USDG", false) ? IERC20(address(new DevUSDG(o.owner))) : IERC20(vm.envAddress("USDG"));
        o.oracle = new PriceOracle(o.owner, vm.envAddress("PRICE_SIGNER"), vm.envAddress("KEEPER"));
        o.exchange = new PExchange(o.owner, o.usdg, o.oracle, vm.envAddress("KEEPER"));
        if (platform == address(0)) platform = address(o.exchange);
        o.factory = new LaunchFactory(o.owner, o.exchange, o.oracle, platform);
        (o.graduator, o.feeVault) = _wire(o.factory, o.exchange, poolManager);
        o.router = new Router(o.usdg, o.exchange, poolManager);
        _configure(o, platform);
        vm.stopBroadcast();
        _write(o);
    }

    function _configure(Out memory o, address platform) internal {
        address keeper = vm.envAddress("KEEPER");
        o.exchange.setRoles(keeper, address(o.factory), vm.envOr("BRIDGE_DEPOSIT", address(0)));
        o.exchange
            .setOutflowCap(vm.envOr("OUTFLOW_FLOOR", uint256(5_000e6)), vm.envOr("OUTFLOW_FLOAT_BPS", uint256(5_000)));
        uint256 gradUsd = vm.envOr("GRAD_USD", uint256(6_000e6));
        if (gradUsd != 6_000e6) o.factory.setConfig(platform, gradUsd);
    }

    function _write(Out memory o) internal {
        string memory key = "deployment";
        vm.serializeAddress(key, "usdg", address(o.usdg));
        vm.serializeAddress(key, "oracle", address(o.oracle));
        vm.serializeAddress(key, "exchange", address(o.exchange));
        vm.serializeAddress(key, "factory", address(o.factory));
        vm.serializeAddress(key, "router", address(o.router));
        vm.serializeAddress(key, "graduator", address(o.graduator));
        vm.serializeAddress(key, "feeVault", address(o.feeVault));
        vm.serializeAddress(key, "poolManager", vm.envOr("POOL_MANAGER", RH_POOL_MANAGER));
        vm.serializeAddress(key, "owner", o.owner);
        string memory json = vm.serializeUint(key, "fromBlock", o.fromBlock);
        string memory path = vm.envOr("DEPLOY_OUT", string.concat("deployments/", vm.toString(block.chainid), ".json"));
        vm.writeJson(json, path);

        console2.log("oracle  ", address(o.oracle));
        console2.log("exchange", address(o.exchange));
        console2.log("factory ", address(o.factory));
        console2.log("router  ", address(o.router));
        console2.log("graduator", address(o.graduator));
        console2.log("feeVault", address(o.feeVault));
        console2.log("usdg    ", address(o.usdg));
        console2.log("written ", path);
    }
}
