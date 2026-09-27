// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {ForkBase} from "./ForkBase.t.sol";

contract ForkSmokeTest is ForkBase {
    function test_wiringMatchesTheDeployScript() public view {
        assertEq(address(exchange.usdg()), USDG);
        assertEq(address(factory.graduator()), address(graduator));
        assertEq(address(factory.feeVault()), address(vault));
        assertEq(uint160(address(graduator)) & 0x3FFF, 0x2000, "hook flags");
        assertEq(oracle.poster(), keeper);
        assertEq(exchange.keeper(), keeper);
        assertEq(usdg.balanceOf(alice), 100_000e6);
    }
}
