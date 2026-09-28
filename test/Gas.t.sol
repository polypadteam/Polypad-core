// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {BondingCurve} from "../src/BondingCurve.sol";
import {Coin} from "../src/Coin.sol";

import {PolypadBase} from "./Polypad.t.sol";

/// @dev Gas of router trades, plain coins vs holder-share coins. Run with -vv.
contract GasTest is PolypadBase {
    function setUp() public override {
        super.setUp();
        vm.prank(owner);
        exchange.setMaxRisk(ID, type(uint256).max);
    }

    function _measure(uint16 bps) internal returns (uint256 buy1, uint256 buy2, uint256 sell, uint256 transfer) {
        (Coin c, BondingCurve curve) = _launch(ID, bps);
        _buy(bob, curve, 100e6);
        vm.warp(block.timestamp + 60);
        uint256 g = gasleft();
        (uint256 got,) = _buy(alice, curve, 100e6);
        buy1 = g - gasleft();
        vm.warp(block.timestamp + 60);
        g = gasleft();
        _buy(alice, curve, 100e6);
        buy2 = g - gasleft();
        vm.warp(block.timestamp + 60);
        g = gasleft();
        _sell(alice, curve, got);
        sell = g - gasleft();
        vm.warp(block.timestamp + 60);
        uint256 half = c.balanceOf(alice) / 2;
        vm.prank(alice);
        g = gasleft();
        c.transfer(bob, half);
        transfer = g - gasleft();
    }

    function test_gas() public {
        (uint256 a, uint256 b, uint256 s, uint256 t) = _measure(0);
        emit log_named_uint("plain  first buy", a);
        emit log_named_uint("plain  buy", b);
        emit log_named_uint("plain  sell", s);
        emit log_named_uint("plain  transfer", t);
        (a, b, s, t) = _measure(5_000);
        emit log_named_uint("holder first buy", a);
        emit log_named_uint("holder buy", b);
        emit log_named_uint("holder sell", s);
        emit log_named_uint("holder transfer", t);
    }
}
