// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {BondingCurve} from "./BondingCurve.sol";
import {Coin} from "./Coin.sol";
import {FeeVault} from "./FeeVault.sol";
import {Fees} from "./Fees.sol";
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
    /// @notice Most a curve may charge per trade: 2%.
    uint16 public constant MAX_CURVE_FEE_BPS = 200;
    /// @notice Most a graduated pool may charge: 1.5% (v4 fee units).
    uint24 public constant MAX_POOL_FEE = 15_000;
    /// @notice The creator side's share of any fee stays between 30% and 80%.
    uint16 public constant MIN_CREATOR_SHARE_BPS = 3_000;
    uint16 public constant MAX_CREATOR_SHARE_BPS = 8_000;

    /// @notice Fees for new launches: 1.4% on the curve and 1% in the pool, each
    ///         split half to the creator side and half to the platform. Each coin
    ///         keeps the fees it launched with.
    Fees public fees = Fees(140, 5_000, 10_000, 5_000);

    /// @notice Where creator fees go. Existing curves keep the one they were made with.
    FeeVault public feeVault;

    address[] public curves;
    mapping(address => bool) public isCurve;

    event Launched(
        uint256 indexed positionId,
        address indexed creator,
        address coin,
        address curve,
        address pToken,
        uint256 launchPrice,
        uint256 phantom,
        uint16 holdersBps
    );
    event ConfigSet(address platform, uint256 gradUsd);
    event GraduatorSet(address graduator);
    event FeeVaultSet(address feeVault);
    event FeesSet(uint16 curveFeeBps, uint16 curveCreatorShareBps, uint24 poolFee, uint16 poolCreatorShareBps);

    error QuoteForOtherMarket(uint256 quoted, uint256 positionId);
    error PriceOutOfBand(uint256 price);
    error NoGraduator();
    error BadHoldersShare(uint16 holdersBps);
    error BadFees();

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

    /// @notice Fees for coins launched from now on, within the caps above.
    function setFees(Fees calldata f) external onlyOwner {
        if (
            f.curveFeeBps == 0 || f.curveFeeBps > MAX_CURVE_FEE_BPS || f.poolFee == 0 || f.poolFee > MAX_POOL_FEE
                || f.curveCreatorShareBps < MIN_CREATOR_SHARE_BPS || f.curveCreatorShareBps > MAX_CREATOR_SHARE_BPS
                || f.poolCreatorShareBps < MIN_CREATOR_SHARE_BPS || f.poolCreatorShareBps > MAX_CREATOR_SHARE_BPS
        ) revert BadFees();
        fees = f;
        emit FeesSet(f.curveFeeBps, f.curveCreatorShareBps, f.poolFee, f.poolCreatorShareBps);
    }

    function setFeeVault(FeeVault feeVault_) external onlyOwner {
        feeVault = feeVault_;
        emit FeeVaultSet(address(feeVault_));
    }

    /**
     * @param holdersBps the part of the creator's fees paid to the coin's holders
     *        instead of the creator, 0 to 10,000. Fixed for the life of the coin.
     */
    function launch(
        uint256 positionId,
        string calldata name,
        string calldata symbol,
        string calldata metadataURI,
        uint16 holdersBps,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) external returns (Coin coin, BondingCurve curve) {
        if (q.positionId != positionId) revert QuoteForOtherMarket(q.positionId, positionId);
        if (address(graduator) == address(0) || address(feeVault) == address(0)) revert NoGraduator();
        if (holdersBps > 10_000) revert BadHoldersShare(holdersBps);
        return _deploy(positionId, _launchPrice(q, sig), name, symbol, metadataURI, holdersBps);
    }

    function _emitLaunched(
        uint256 positionId,
        Coin coin,
        BondingCurve curve,
        PToken p,
        uint256 price,
        uint16 holdersBps
    ) internal {
        emit Launched(
            positionId, msg.sender, address(coin), address(curve), address(p), price, curve.phantom(), holdersBps
        );
    }

    function _deploy(
        uint256 positionId,
        uint256 price,
        string calldata name,
        string calldata symbol,
        string calldata metadataURI,
        uint16 holdersBps
    ) internal returns (Coin coin, BondingCurve curve) {
        PToken p = exchange.ensurePToken(positionId);
        // Target in shares = gradUsd / price; phantom = 0.4x target.
        curve = new BondingCurve(
            IERC20(address(p)),
            address(exchange),
            msg.sender,
            platform,
            (gradUsd * 2e6) / (price * 5),
            graduator,
            feeVault,
            fees
        );
        coin = new Coin(name, symbol, metadataURI, address(curve), holdersBps > 0 ? address(feeVault) : address(0));
        feeVault.register(address(coin), address(curve), address(graduator), address(p), msg.sender, holdersBps);
        curve.initialize(IERC20(address(coin)));
        curves.push(address(curve));
        isCurve[address(curve)] = true;
        _emitLaunched(positionId, coin, curve, p, price, holdersBps);
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
