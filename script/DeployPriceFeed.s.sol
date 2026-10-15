// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script, console2} from "forge-std/Script.sol";

import {PriceFeed} from "../src/PriceFeed.sol";

/**
 * Deploys the PriceFeed on its own (it is not wired into the other contracts).
 *
 * Env:
 *   KEEPER     the poster (Polypad's keeper)
 *   NEW_OWNER  optional: the owner; defaults to the deploying address
 */
contract DeployPriceFeed is Script {
    function run() external returns (PriceFeed feed) {
        vm.startBroadcast();
        (, address sender,) = vm.readCallers();
        feed = new PriceFeed(vm.envOr("NEW_OWNER", sender), vm.envAddress("KEEPER"));
        vm.stopBroadcast();
        console2.log("PriceFeed", address(feed));
    }
}
