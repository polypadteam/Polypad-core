// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

interface IHolderBook {
    function onTransfer(address from, address to) external;
}

/**
 * @title Coin
 * @notice A launched memecoin. Fixed 1B supply, all minted to its bonding curve.
 *
 * A coin whose creator shares fees with holders tells the FeeVault about every
 * transfer, so the vault can track who holds it and for how long. Other coins
 * are plain ERC20s.
 */
contract Coin is ERC20 {
    uint256 public constant SUPPLY = 1_000_000_000e18;

    /// @notice Image and description, usually an ipfs:// URI.
    string public metadataURI;
    /// @notice The FeeVault paying this coin's holders, or zero if holders get no share.
    address public immutable holderBook;

    constructor(
        string memory name_,
        string memory symbol_,
        string memory metadataURI_,
        address curve,
        address holderBook_
    ) ERC20(name_, symbol_) {
        metadataURI = metadataURI_;
        holderBook = holderBook_;
        _mint(curve, SUPPLY);
    }

    /// @dev Deliberately not try/catch: a notification that could be made to fail
    ///      (by starving it of gas) would let a holder move coins the vault never saw.
    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (holderBook != address(0)) IHolderBook(holderBook).onTransfer(from, to);
    }
}
