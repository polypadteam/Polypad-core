// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/**
 * @title PToken
 * @notice One Polymarket outcome share on Robinhood Chain. 1 token = 1 share.
 *
 * Six decimals because CTF shares are six-decimal, so a supply of 1e6 means one
 * share and no scaling factor sits between this token and the desk's holdings.
 *
 * Only the exchange that deployed it can mint or burn. The exchange mints when
 * someone pays USDG at the oracle price and burns when they sell back, and the
 * desk keeps real shares on Polymarket equal to `totalSupply()`.
 */
contract PToken is ERC20 {
    /// @notice The CTF position id this token tracks. It is also the CLOB token id.
    uint256 public immutable positionId;

    /// @notice The only address that may mint or burn.
    address public immutable exchange;

    error OnlyExchange();

    constructor(uint256 positionId_) ERC20("Polypad Share", "pSHARE") {
        positionId = positionId_;
        exchange = msg.sender;
    }

    function decimals() public pure override returns (uint8) {
        return 6;
    }

    function mint(address to, uint256 amount) external {
        if (msg.sender != exchange) revert OnlyExchange();
        _mint(to, amount);
    }

    /// @dev Burns only from the exchange's own balance or a holder that sent it,
    ///      never through an allowance. The exchange calls this on tokens it
    ///      has already taken custody of.
    function burn(address from, uint256 amount) external {
        if (msg.sender != exchange) revert OnlyExchange();
        _burn(from, amount);
    }
}
