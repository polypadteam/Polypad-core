// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/**
 * @title Coin
 * @notice A launched memecoin. Fixed 1B supply, all minted to its bonding curve.
 */
contract Coin is ERC20 {
    uint256 public constant SUPPLY = 1_000_000_000e18;

    /// @notice Image and description, usually an ipfs:// URI.
    string public metadataURI;

    constructor(string memory name_, string memory symbol_, string memory metadataURI_, address curve)
        ERC20(name_, symbol_)
    {
        metadataURI = metadataURI_;
        _mint(curve, SUPPLY);
    }
}
