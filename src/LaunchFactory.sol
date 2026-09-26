// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {BondingCurve} from "./BondingCurve.sol";
import {Coin} from "./Coin.sol";
import {Graduator} from "./Graduator.sol";
import {PExchange} from "./PExchange.sol";
import {PriceOracle} from "./PriceOracle.sol";
import {PToken} from "./PToken.sol";

/**
 * @title LaunchFactory
 * @notice Launches a coin on a Polymarket outcome. Free; the creator pays gas.
 *
 * A launch needs a live BUY quote from the pricer inside the exchange's band;
 * that quote sets the curve's size. The first coin on a market also creates
 * that market's pToken.
 *
 * The graduation target is set in dollars and converted to shares at the launch
 * price, so every curve raises about `gradUsd` if the odds do not move. The
 * phantom reserve is 0.4x the target in shares, the same ratio as WORM.
 */
contract LaunchFactory is Ownable2Step {
    PExchange public immutable exchange;
    PriceOracle public immutable oracle;

    address public platform;
    /// @notice Graduation target in USDG, 6 decimals.
    uint256 public gradUsd = 6_000e6;

    /// @notice Where new curves graduate. Existing curves keep the one they were made with.
    Graduator public graduator;

    address[] public curves;
    mapping(address => bool) public isCurve;

    event Launched(
        uint256 indexed positionId,
        address indexed creator,
        address coin,
        address curve,
        address pToken,
        uint256 launchPrice,
        uint256 phantom
    );
    event ConfigSet(address platform, uint256 gradUsd);
    event GraduatorSet(address graduator);

    error QuoteForOtherMarket(uint256 quoted, uint256 positionId);
    error PriceOutOfBand(uint256 price);
    error NoGraduator();

    constructor(address owner_, PExchange exchange_, PriceOracle oracle_, address platform_) Ownable(owner_) {
        exchange = exchange_;
        oracle = oracle_;
        platform = platform_;
    }

    function setConfig(address platform_, uint256 gradUsd_) external onlyOwner {
        platform = platform_;
        gradUsd = gradUsd_;
        emit ConfigSet(platform_, gradUsd_);
    }

    function setGraduator(Graduator graduator_) external onlyOwner {
        graduator = graduator_;
        emit GraduatorSet(address(graduator_));
    }

    function launch(
        uint256 positionId,
        string calldata name,
        string calldata symbol,
        string calldata metadataURI,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) external returns (Coin coin, BondingCurve curve) {
        if (q.positionId != positionId) revert QuoteForOtherMarket(q.positionId, positionId);
        if (address(graduator) == address(0)) revert NoGraduator();
        uint256 price = _launchPrice(q, sig);
        PToken p = exchange.ensurePToken(positionId);

        // Target in shares = gradUsd / price; phantom = 0.4x target.
        curve = new BondingCurve(
            IERC20(address(p)), address(exchange), msg.sender, platform, (gradUsd * 2e6) / (price * 5), graduator
        );
        coin = new Coin(name, symbol, metadataURI, address(curve));
        curve.initialize(IERC20(address(coin)));
        curves.push(address(curve));
        isCurve[address(curve)] = true;

        emit Launched(positionId, msg.sender, address(coin), address(curve), address(p), price, curve.phantom());
    }

    function _launchPrice(PriceOracle.Quote calldata q, bytes calldata sig) internal view returns (uint256) {
        uint64 price = oracle.verify(q, sig, oracle.BUY());
        if (price < exchange.minPrice() || price > exchange.maxPriceOf(q.positionId)) revert PriceOutOfBand(price);
        return price;
    }

    function curveCount() external view returns (uint256) {
        return curves.length;
    }
}
