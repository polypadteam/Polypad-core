// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script, console2} from "forge-std/Script.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

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
 *   PLATFORM         receives the platform fee
 *   USDG             collateral; omit with DEV_USDG=1 to deploy a dev token
 *   BRIDGE_DEPOSIT   the desk's Polymarket deposit address (optional; set later with setRoles)
 *   GRAD_USD         graduation target in USDG units (default 6000e6)
 *   DEPLOY_OUT       output path (default deployments/<chainid>.json; set it for local runs)
 *
 * The broadcaster becomes the owner of the oracle, exchange and factory.
 * Writes the addresses to DEPLOY_OUT.
 *
 * Verify every contract on Sourcify (Blockscout shows Sourcify sources):
 *   forge script script/Deploy.s.sol --broadcast --verify --verifier sourcify ...
 */
contract Deploy is Script {
    function run() external {
        address priceSigner = vm.envAddress("PRICE_SIGNER");
        address keeper = vm.envAddress("KEEPER");
        address platform = vm.envAddress("PLATFORM");
        address bridgeDeposit = vm.envOr("BRIDGE_DEPOSIT", address(0));
        uint256 gradUsd = vm.envOr("GRAD_USD", uint256(6_000e6));

        vm.startBroadcast();
        address owner = msg.sender;
        uint256 fromBlock = block.number;

        IERC20 usdg = vm.envOr("DEV_USDG", false)
            ? IERC20(address(new DevUSDG(owner)))
            : IERC20(vm.envAddress("USDG"));

        PriceOracle oracle = new PriceOracle(owner, priceSigner, keeper);
        PExchange exchange = new PExchange(owner, usdg, oracle, keeper);
        LaunchFactory factory = new LaunchFactory(owner, exchange, oracle, platform);
        Router router = new Router(usdg, exchange);
        exchange.setRoles(keeper, address(factory), bridgeDeposit);
        oracle.setPoster(keeper);
        if (gradUsd != 6_000e6) factory.setConfig(platform, gradUsd);
        vm.stopBroadcast();

        string memory key = "deployment";
        vm.serializeAddress(key, "usdg", address(usdg));
        vm.serializeAddress(key, "oracle", address(oracle));
        vm.serializeAddress(key, "exchange", address(exchange));
        vm.serializeAddress(key, "factory", address(factory));
        vm.serializeAddress(key, "router", address(router));
        vm.serializeAddress(key, "owner", owner);
        string memory json = vm.serializeUint(key, "fromBlock", fromBlock);
        string memory path = vm.envOr("DEPLOY_OUT", string.concat("deployments/", vm.toString(block.chainid), ".json"));
        vm.writeJson(json, path);

        console2.log("oracle  ", address(oracle));
        console2.log("exchange", address(exchange));
        console2.log("factory ", address(factory));
        console2.log("router  ", address(router));
        console2.log("usdg    ", address(usdg));
        console2.log("written ", path);
    }
}
