// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {SafeCast} from "@openzeppelin/contracts/utils/math/SafeCast.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

import {PToken} from "./PToken.sol";
import {PriceOracle} from "./PriceOracle.sol";

/**
 * @title PExchange
 * @notice Turns USDG into Polymarket shares and back, instantly, at a signed quote.
 *
 * The contract holds one shared USDG float for every market. A buyer's USDG
 * lands here and pToken is minted to them at `buy quote x (1 + buySpread)`; the
 * desk then buys the real share on Polymarket. A seller's pToken is burned and
 * USDG is paid out of the float at `sell quote x (1 - sellSpread)`; the desk sells
 * the share. Quotes are signed by the pricer and checked by `PriceOracle`. Money moves between the float and the desk's Polymarket balance
 * through bridge.polymarket.com, never on a user's transaction.
 *
 * ## What backs a pToken
 *
 * Real shares in the desk's Polymarket wallet, plus shares the desk is still
 * buying. The keeper reports the held amount as `backed`, never more than the
 * supply, and every burn lowers it at once, so shares the desk has yet to sell
 * after a redemption cannot be minted again unhedged. Minting stops for a
 * market once the unbacked supply (`totalSupply - backed`) times what a share
 * could still rise by (`1 - price`) would pass `maxRisk`, in USDG. The price
 * is the one each unbacked share was minted at (`unbackedRisk` carries the
 * total), so minting at rising prices cannot re-value cheaper ones. That caps
 * what a missed fill or a stale price can cost per market, and lets the desk
 * leave near-certain shares (a 98c market risks 2c a share) unhedged.
 *
 * ## Guards on a mint
 *
 * - a live BUY quote for this market, covering this size
 * - market not paused and not settled
 * - price inside [minPrice, maxPriceOf(market)] (5c..98c by default; the owner
 *   can lift one market's ceiling up to 99c)
 * - unbacked risk under the cap
 *
 * Redeeming needs a live SELL quote, or nothing once the market has settled:
 * holders can always sell a paused market, and after resolution they are paid
 * the payout less `settleFeeBps` (which pays for bridging the desk's winnings
 * back).
 *
 * ## After settlement
 *
 * A market that settled with a payout keeps trading: mints need no quote and
 * cost the payout plus the buy spread. Those pTokens are backed by the USDG
 * paid for them, which stays in the float, so no shares are bought. A coin on a
 * market that resolved YES becomes a coin paired with a $1 token and trades on.
 *
 * ## Redemption queue
 *
 * A redemption never fails for lack of float. The seller is paid whatever the
 * float (less what is already queued) holds right away, the pToken is burned,
 * and any remainder becomes a claim for the exact USDG still owed, paid first
 * in, first out as soon as the float refills: by anyone calling `payQueue`,
 * and by every mint. The keeper
 * bridges from the desk as soon as a claim appears. `sendToBridge` can never
 * touch USDG owed to the queue.
 *
 * ## Circuit breaker
 *
 * USDG owed by redemptions is metered over a sliding hour: the previous clock
 * hour counts for the part of it still inside the last hour (so up to about two
 * caps can leave within 3,600 seconds across a boundary). It is gross, not net
 * of mints: crediting mints would let a stolen signing key spend honest buyers'
 * money on top of the cap. So a buy and sale back to back does use the cap, and
 * can push honest sellers into the delay for the spread it costs. The hourly cap
 * scales with the float: `outflowFloatBps` of the float (less the queue) as it
 * stood when the clock hour began, before any of its trades (or when the one
 * before it began, if that was less), and never less than `outflowFloor`.
 * Trades inside the hour do not move it, so neither selling nor minting and
 * selling in a loop can raise it, and money put in just before an hour turns
 * cannot lift that hour's cap above the float at the last snapshot. A sale is never refused for the cap (unless
 * the part over it is under `MIN_DELAYED`: dust tickets are refused). The part
 * over it is delayed instead: its pToken is held here and its price is fixed,
 * and it is not reserved in the float until released. After
 * `DELAY` anyone can `release` it into the queue. Until then the owner can
 * `cancel` it, which gives the pToken to the sale's payee. So a leaked signing key or a
 * pricing bug cannot take more than the cap in an hour, and what it tries past
 * that waits where the owner can stop it; an honest seller past the cap gets
 * their price, an hour later. The keeper (or owner) can `halt` every mint,
 * redeem, release and queue payment at once; only the owner lifts it.
 */
contract PExchange is Ownable2Step, ReentrancyGuard {
    using SafeERC20 for IERC20;

    uint256 internal constant ONE = 1e6;
    uint256 internal constant BPS = 10_000;

    IERC20 public immutable usdg;
    PriceOracle public immutable oracle;

    /// @notice May report backing and send float to the bridge.
    address public keeper;
    /// @notice May create pTokens for new markets.
    address public factory;
    /// @notice Polymarket deposit address for the desk wallet. Float sent here arrives as pUSD.
    address public bridgeDeposit;
    /// @notice Delay before a changed bridge deposit takes effect.
    uint256 public constant BRIDGE_DELAY = 2 days;
    address public pendingBridgeDeposit;
    uint256 public pendingBridgeDepositAt;

    uint16 public buySpreadBps = 25;
    uint16 public sellSpreadBps = 25;
    uint64 public minPrice = 50_000; // 5c
    uint64 public maxPrice = 980_000; // 98c
    /// @notice USDG a market's unbacked supply may lose if its price went to $1
    ///         (unbacked x (1 - price)), unless overridden. 6 decimals.
    uint256 public defaultMaxRisk = 1_000e6;

    mapping(uint256 positionId => PToken) public pTokenOf;
    mapping(address pToken => uint256) public positionIdOf;
    mapping(uint256 positionId => uint256) public backed;
    mapping(uint256 positionId => uint256) public maxRiskOverride;

    /// @notice The hourly redemption cap is never below this, USDG.
    uint256 public outflowFloor = 5_000e6;
    /// @notice The hourly redemption cap as a share of the float, bps (5_000 = 50%).
    ///         At most 100%: a cap above the float would never bind.
    uint256 public outflowFloatBps = 5_000;
    /// @notice How long the part of a sale over the cap waits before `release`.
    uint256 public constant DELAY = 1 hours;
    /// @notice Every mint and redeem refused while set.
    bool public halted;
    /// @notice Fee on redemptions of a settled market, bps of the payout.
    uint16 public settleFeeBps = 50;
    /// @notice Per-market mint ceiling above `maxPrice`, for near-certain markets. 0 = default.
    mapping(uint256 positionId => uint64) public maxPriceOverride;

    struct Claim {
        address to;
        uint96 amount;
    }

    /// @notice Redemptions waiting for float, oldest first from `claimHead`.
    Claim[] public claims;
    uint256 public claimHead;
    /// @notice USDG owed to the queue, including claims that could not be delivered.
    uint256 public queued;
    /// @notice Claims whose transfer failed (a frozen or rejecting address): the
    ///         owner withdraws them with `withdrawUnclaimed`, so one bad recipient
    ///         never blocks the queue behind it.
    mapping(address => uint256) public unclaimed;
    /// @notice Sum of `unclaimed`: reserved for those owners, never paid to the queue.
    uint256 public unclaimedTotal;
    /// @notice How much of each signed quote (by EIP-712 digest) has been used.
    mapping(bytes32 digest => uint256) public quoteFilled;
    /// @dev Set once a bridge deposit address has been in place; later ones wait BRIDGE_DELAY.
    bool public bridgeEverSet;

    mapping(uint256 hour => uint256) public outflowInHour;
    /// @notice The smallest part of a sale that may be delayed; below it the sale reverts.
    uint256 public constant MIN_DELAYED = 1e6;

    /// @dev The free float at the first mint or redeem of `hour`, before that
    ///      trade moved it (`amount`), and the hour's cap base: the lower of it
    ///      and the last such snapshot (`base`).
    struct HourFloat {
        uint64 hour;
        uint96 amount;
        uint96 base;
    }

    HourFloat internal hourFloat;

    /// @notice The part of a sale over the hourly cap: `pAmount` pToken held here,
    ///         owed `amount` USDG once released. Deleted when released or cancelled.
    struct DelayedSale {
        address to;
        uint64 readyAt;
        uint256 positionId;
        uint128 pAmount;
        uint128 amount;
    }

    DelayedSale[] public delayed;
    /// @notice pToken held here for delayed sales, per market: `absorb` leaves it alone.
    mapping(uint256 positionId => uint256) public delayedShares;
    /// @notice The unbacked supply's risk for the cap, packed: the low 192 bits
    ///         are what it could cost to cover at $1, as the mints that made it
    ///         priced it (shares x (1 - price), 12 decimals); the top 64 the
    ///         lowest price minted at since the market was last fully backed
    ///         (0 = unknown, taken as $0).
    mapping(uint256 positionId => uint256) public unbackedRisk;

    event PTokenCreated(uint256 indexed positionId, address pToken);
    event Minted(uint256 indexed positionId, address indexed to, uint256 usdgIn, uint256 pOut, uint256 price);
    event Redeemed(uint256 indexed positionId, address indexed to, uint256 pIn, uint256 usdgOut, uint256 price);
    event Absorbed(uint256 indexed positionId, uint256 amount);
    event BackedReported(uint256 indexed positionId, uint256 amount);
    event SentToBridge(address indexed to, uint256 amount);
    event ParamsSet(
        uint16 buySpreadBps, uint16 sellSpreadBps, uint64 minPrice, uint64 maxPrice, uint256 defaultMaxRisk
    );
    event RolesSet(address keeper, address factory, address bridgeDeposit);
    event BridgeDepositProposed(address indexed bridgeDeposit, uint256 effectiveAt);
    event Rescued(address indexed token, address indexed to, uint256 amount);
    event OutflowCapSet(uint256 floor, uint256 floatBps);
    event SaleDelayed(
        uint256 indexed ticket,
        uint256 indexed positionId,
        address indexed to,
        uint256 pAmount,
        uint256 amount,
        uint256 readyAt
    );
    event Released(uint256 indexed ticket, uint256 indexed positionId);
    event Cancelled(uint256 indexed ticket);
    event HaltSet(bool halted);
    event Queued(uint256 indexed ticket, address indexed to, uint256 amount);
    event ClaimPaid(uint256 indexed ticket, address indexed to, uint256 amount);
    event ClaimUndelivered(uint256 indexed ticket, address indexed to, uint256 amount);
    event UnclaimedWithdrawn(address indexed from, address indexed to, uint256 amount);
    event SettleFeeSet(uint16 settleFeeBps);
    event MaxPriceSet(uint256 indexed positionId, uint64 maxPrice);
    event ShareLabelSet(uint256 indexed positionId, string name, string symbol);

    error OnlyKeeper();
    error OnlyFactory();
    error UnknownPToken(address pToken);
    error QuoteForOtherMarket(uint256 quoted, uint256 positionId);
    error QuoteTooSmall(uint256 amount, uint256 maxAmount);
    error PriceOutOfBand(uint256 price);
    error RiskCap(uint256 risk, uint256 cap);
    error InsufficientFloat(uint256 wanted, uint256 available);
    error Slippage(uint256 got, uint256 minimum);
    error ZeroAmount();
    error BadParams();
    error NoBridge();
    error Halted();
    error BridgeChangeDelayed();
    error NotYet(uint256 effectiveAt);
    error NothingPending();
    error NotRescuable(address token);
    error SettledAtZero(uint256 positionId);
    error DustOverCap(uint256 amount);

    modifier onlyKeeper() {
        if (msg.sender != keeper) revert OnlyKeeper();
        _;
    }

    constructor(address owner_, IERC20 usdg_, PriceOracle oracle_, address keeper_) Ownable(owner_) {
        usdg = usdg_;
        oracle = oracle_;
        keeper = keeper_;
    }

    /* ------------------------------------------------------------ markets */

    /// @notice The pToken for a market, created on first use at a CREATE2 address.
    function ensurePToken(uint256 positionId) external returns (PToken p) {
        if (msg.sender != factory && msg.sender != owner()) revert OnlyFactory();
        p = pTokenOf[positionId];
        if (address(p) != address(0)) return p;
        p = new PToken{salt: bytes32(positionId)}(positionId);
        pTokenOf[positionId] = p;
        positionIdOf[address(p)] = positionId;
        emit PTokenCreated(positionId, address(p));
    }

    /// @notice Where a market's pToken lives or will live.
    function pTokenAddress(uint256 positionId) external view returns (address) {
        bytes32 codeHash = keccak256(abi.encodePacked(type(PToken).creationCode, abi.encode(positionId)));
        return address(
            uint160(uint256(keccak256(abi.encodePacked(bytes1(0xff), address(this), bytes32(positionId), codeHash))))
        );
    }

    /* -------------------------------------------------------- mint/redeem */

    /**
     * @notice Pay USDG, receive pToken at the quoted buy price plus the buy spread.
     * @param usdgIn USDG to spend, 6 decimals
     * @param minOut revert if fewer pToken would be minted
     * @param q a BUY quote for this market with `maxAmount >= usdgIn`
     */
    function mint(
        address pToken,
        uint256 usdgIn,
        uint256 minOut,
        address to,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) external nonReentrant returns (uint256 out) {
        if (usdgIn == 0) revert ZeroAmount();
        (uint256 id, PToken p) = _market(pToken);
        (, bool settled, uint64 payout) = oracle.status(id);
        if (settled) return _mintSettled(id, p, usdgIn, minOut, to, payout);
        uint64 price = _quoted(id, usdgIn, q, sig, oracle.BUY());
        out = _mint(id, p, usdgIn, minOut, to, price, buySpreadBps);
    }

    /**
     * @notice Burn pToken, receive USDG at the quoted sell price minus the sell
     *         spread, or at the payout once the market has settled (quote ignored).
     * @param q a SELL quote for this market with `maxAmount >= amountIn`
     */
    function redeem(
        address pToken,
        uint256 amountIn,
        uint256 minOut,
        address to,
        PriceOracle.Quote calldata q,
        bytes calldata sig
    ) external nonReentrant returns (uint256 out) {
        if (amountIn == 0) revert ZeroAmount();
        (uint256 id, PToken p) = _market(pToken);
        (, bool settled, uint64 payout) = oracle.status(id);
        uint64 price = settled ? payout : _quoted(id, amountIn, q, sig, oracle.SELL());
        out = _redeem(id, p, amountIn, minOut, to, price, settled ? settleFeeBps : sellSpreadBps);
    }

    /**
     * @notice Burn pToken this contract holds. Curves send the float share of
     *         their fees here; burning it lowers supply, the desk sells the
     *         matching shares, and the proceeds land in the Polymarket balance.
     *         The USDG those shares were bought with already sits in the float.
     */
    function absorb(address pToken) external returns (uint256 amount) {
        (uint256 id, PToken p) = _market(pToken);
        amount = p.balanceOf(address(this)) - delayedShares[id];
        if (amount == 0) return 0;
        p.burn(address(this), amount);
        _unback(id, amount);
        emit Absorbed(id, amount);
    }

    /**
     * @notice Pay up to `max` queued claims, oldest first, while the float covers
     *         them. Anyone may call.
     */
    function payQueue(uint256 max) external nonReentrant returns (uint256 paid) {
        if (halted) revert Halted();
        return _payQueue(max);
    }

    /// @notice Pay a claim the queue could not deliver to the address it was owed
    ///         to (never elsewhere: that would route around a USDG freeze). Anyone
    ///         may call, for a recipient that cannot call itself.
    function withdrawUnclaimed(address to) external nonReentrant returns (uint256 amount) {
        amount = unclaimed[to];
        if (amount == 0) revert ZeroAmount();
        unclaimed[to] = 0;
        unclaimedTotal -= amount;
        queued -= amount;
        usdg.safeTransfer(to, amount);
        emit UnclaimedWithdrawn(to, to, amount);
    }

    /// @notice Claims still waiting.
    function queueLength() external view returns (uint256) {
        return claims.length - claimHead;
    }

    /**
     * @notice Pay a delayed sale once `DELAY` has passed: its pToken is burned
     *         and its USDG is paid, or queued if the float is short. Anyone may call.
     */
    function release(uint256 ticket) external nonReentrant {
        if (halted) revert Halted();
        DelayedSale memory d = delayed[ticket];
        if (d.to == address(0)) revert NothingPending();
        if (block.timestamp < d.readyAt) revert NotYet(d.readyAt);
        delete delayed[ticket];
        delayedShares[d.positionId] -= d.pAmount;
        pTokenOf[d.positionId].burn(address(this), d.pAmount);
        _unback(d.positionId, d.pAmount);
        emit Released(ticket, d.positionId);
        _pay(d.to, d.amount, true);
    }

    function delayedLength() external view returns (uint256) {
        return delayed.length;
    }

    /// @notice Undo a delayed sale before it is released: the pToken goes back to
    ///         the sale's payee (`to`).
    function cancel(uint256 ticket) external onlyOwner {
        DelayedSale memory d = delayed[ticket];
        if (d.to == address(0)) revert NothingPending();
        delete delayed[ticket];
        delayedShares[d.positionId] -= d.pAmount;
        IERC20(address(pTokenOf[d.positionId])).safeTransfer(d.to, d.pAmount);
        emit Cancelled(ticket);
    }

    /* -------------------------------------------------------------- keeper */

    /// @notice Shares the desk holds on Polymarket, per market.
    function reportBacked(uint256[] calldata ids, uint256[] calldata amounts) external onlyKeeper {
        if (ids.length != amounts.length) revert BadParams();
        for (uint256 i; i < ids.length; ++i) {
            // Shares beyond supply back nothing: counting them would let the
            // excess be minted unhedged (or, from a stolen keeper key, any amount).
            PToken p = pTokenOf[ids[i]];
            uint256 supply = address(p) == address(0) ? 0 : p.totalSupply();
            uint256 amount = amounts[i] < supply ? amounts[i] : supply;
            // Shares that stop being backed carry risk again, at the most a share can.
            uint256 was = backed[ids[i]];
            if (amount < was) unbackedRisk[ids[i]] = uint192(unbackedRisk[ids[i]]) + (was - amount) * ONE;
            backed[ids[i]] = amount;
            emit BackedReported(ids[i], amount);
        }
    }

    /// @notice Move float to the desk's Polymarket deposit address. Destination is fixed by the owner.
    function sendToBridge(uint256 amount) external onlyKeeper nonReentrant {
        if (bridgeDeposit == address(0)) revert NoBridge();
        uint256 free = freeFloat();
        if (amount > free) revert InsufficientFloat(amount, free);
        usdg.safeTransfer(bridgeDeposit, amount);
        emit SentToBridge(bridgeDeposit, amount);
    }

    /* --------------------------------------------------------------- owner */

    /**
     * @notice Set the keeper and factory. The bridge deposit can be set here only
     *         the first time; after that it changes through
     *         `proposeBridgeDeposit` with a `BRIDGE_DELAY`, since it is where the
     *         float may be sent.
     */
    function setRoles(address keeper_, address factory_, address bridgeDeposit_) external onlyOwner {
        if (bridgeDeposit_ != bridgeDeposit) {
            // Clearing is instant; only the very first bridge skips BRIDGE_DELAY,
            // so a clear followed by a set cannot install a new one at once.
            if (bridgeDeposit_ != address(0)) {
                if (bridgeEverSet) revert BridgeChangeDelayed();
                bridgeEverSet = true;
            }
            bridgeDeposit = bridgeDeposit_;
        }
        keeper = keeper_;
        factory = factory_;
        emit RolesSet(keeper_, factory_, bridgeDeposit);
    }

    function proposeBridgeDeposit(address bridgeDeposit_) external onlyOwner {
        pendingBridgeDeposit = bridgeDeposit_;
        pendingBridgeDepositAt = block.timestamp + BRIDGE_DELAY;
        emit BridgeDepositProposed(bridgeDeposit_, pendingBridgeDepositAt);
    }

    function acceptBridgeDeposit() external onlyOwner {
        if (pendingBridgeDepositAt == 0) revert NothingPending();
        if (block.timestamp < pendingBridgeDepositAt) revert NotYet(pendingBridgeDepositAt);
        bridgeDeposit = pendingBridgeDeposit;
        if (bridgeDeposit != address(0)) bridgeEverSet = true;
        pendingBridgeDeposit = address(0);
        pendingBridgeDepositAt = 0;
        emit RolesSet(keeper, factory, bridgeDeposit);
    }

    function setParams(
        uint16 buySpreadBps_,
        uint16 sellSpreadBps_,
        uint64 minPrice_,
        uint64 maxPrice_,
        uint256 defaultMaxRisk_
    ) external onlyOwner {
        if (buySpreadBps_ > 500 || sellSpreadBps_ > 500 || minPrice_ >= maxPrice_ || maxPrice_ >= ONE) {
            revert BadParams();
        }
        buySpreadBps = buySpreadBps_;
        sellSpreadBps = sellSpreadBps_;
        minPrice = minPrice_;
        maxPrice = maxPrice_;
        defaultMaxRisk = defaultMaxRisk_;
        emit ParamsSet(buySpreadBps_, sellSpreadBps_, minPrice_, maxPrice_, defaultMaxRisk_);
    }

    function setMaxRisk(uint256 positionId, uint256 cap) external onlyOwner {
        maxRiskOverride[positionId] = cap;
    }

    /// @notice Name a market's share token after its outcome. Keeper or owner.
    function setShareLabel(uint256 positionId, string calldata name_, string calldata symbol_) external {
        if (msg.sender != keeper && msg.sender != owner()) revert OnlyKeeper();
        PToken p = pTokenOf[positionId];
        if (address(p) == address(0) || bytes(name_).length > 64 || bytes(symbol_).length > 16) revert BadParams();
        p.setLabel(name_, symbol_);
        emit ShareLabelSet(positionId, name_, symbol_);
    }

    /// @notice Lift (or reset, with 0) one market's mint ceiling. At most 99c.
    function setMaxPrice(uint256 positionId, uint64 maxPrice_) external onlyOwner {
        if (maxPrice_ > 990_000 || (maxPrice_ != 0 && maxPrice_ <= minPrice)) revert BadParams();
        maxPriceOverride[positionId] = maxPrice_;
        emit MaxPriceSet(positionId, maxPrice_);
    }

    function setSettleFee(uint16 settleFeeBps_) external onlyOwner {
        if (settleFeeBps_ > 200) revert BadParams();
        settleFeeBps = settleFeeBps_;
        emit SettleFeeSet(settleFeeBps_);
    }

    /// @notice The hourly redemption cap: `floatBps` of the float, never below `floor`.
    function setOutflowCap(uint256 floor, uint256 floatBps) external onlyOwner {
        if (floatBps > BPS) revert BadParams();
        outflowFloor = floor;
        outflowFloatBps = floatBps;
        emit OutflowCapSet(floor, floatBps);
    }

    /// @notice Stop every mint, redeem, release and queue payment. The keeper may halt; only the owner resumes.
    function halt() external {
        if (msg.sender != keeper && msg.sender != owner()) revert OnlyKeeper();
        halted = true;
        emit HaltSet(true);
    }

    function resume() external onlyOwner {
        halted = false;
        emit HaltSet(false);
    }

    /// @notice Return tokens sent here by mistake. Never the float (USDG) or a pToken.
    function rescue(IERC20 token, address to, uint256 amount) external onlyOwner {
        if (address(token) == address(usdg) || positionIdOf[address(token)] != 0) revert NotRescuable(address(token));
        token.safeTransfer(to, amount);
        emit Rescued(address(token), to, amount);
    }

    /* --------------------------------------------------------------- views */

    function maxPriceOf(uint256 positionId) public view returns (uint64) {
        uint64 o = maxPriceOverride[positionId];
        return o == 0 ? maxPrice : o;
    }

    /// @notice Float not owed to the queue: what can pay a redemption now or go to the bridge.
    function freeFloat() public view returns (uint256) {
        uint256 bal = usdg.balanceOf(address(this));
        return bal > queued ? bal - queued : 0;
    }

    function maxRisk(uint256 positionId) public view returns (uint256) {
        uint256 o = maxRiskOverride[positionId];
        return o == 0 ? defaultMaxRisk : o;
    }

    /// @notice pToken minted for `usdgIn` at a quoted buy price.
    function mintOut(uint64 price, uint256 usdgIn) public view returns (uint256) {
        return _mintOut(price, usdgIn, buySpreadBps);
    }

    /// @notice USDG paid for `amountIn` pToken at a quoted sell price.
    function redeemOut(uint64 price, uint256 amountIn) public view returns (uint256) {
        return _redeemOut(price, amountIn, sellSpreadBps);
    }

    /// @notice USDG that may still leave through redemptions this hour.
    function outflowRemaining() public view returns (uint256) {
        // A sliding hour: the previous clock hour counts for the part of it still
        // inside the last 3,600 seconds, so the cap cannot be taken twice across
        // an hour boundary.
        uint256 hour = block.timestamp / 3_600;
        uint256 intoHour = block.timestamp % 3_600;
        uint256 used = _sliding(outflowInHour, hour, intoHour);
        HourFloat memory f = hourFloat;
        uint256 cap = ((f.hour == hour ? f.base : _base(f, freeFloat())) * outflowFloatBps) / BPS;
        if (cap < outflowFloor) cap = outflowFloor;
        return used >= cap ? 0 : cap - used;
    }

    /* ------------------------------------------------------------ internal */

    function _sliding(mapping(uint256 => uint256) storage m, uint256 hour, uint256 intoHour)
        internal
        view
        returns (uint256)
    {
        uint256 prev = hour == 0 ? 0 : m[hour - 1];
        return m[hour] + (prev * (3_600 - intoHour)) / 3_600;
    }

    /// @dev Burned shares no longer need backing: lower it with them, so the gap
    ///      until the desk reports again cannot be minted unhedged.
    function _unback(uint256 id, uint256 n) internal {
        uint256 b = backed[id];
        backed[id] = b > n ? b - n : 0;
    }

    /// @dev Record the float for the outflow cap at the hour's first trade, before it moves it.
    function _markHour() internal {
        uint256 hour = block.timestamp / 3_600;
        HourFloat memory f = hourFloat;
        if (f.hour == hour) return;
        uint256 cur = freeFloat();
        hourFloat = HourFloat(uint64(hour), SafeCast.toUint96(cur), SafeCast.toUint96(_base(f, cur)));
    }

    /// @dev A deposit counts toward the cap only once it has sat through an hour's turn.
    function _base(HourFloat memory f, uint256 cur) internal pure returns (uint256) {
        return f.hour == 0 || cur < f.amount ? cur : f.amount;
    }

    function _mint(uint256 id, PToken p, uint256 usdgIn, uint256 minOut, address to, uint64 price, uint16 spreadBps)
        internal
        returns (uint256 out)
    {
        if (halted) revert Halted();
        if (to == address(0)) revert BadParams();
        _markHour();
        if (price < minPrice || price > maxPriceOf(id)) revert PriceOutOfBand(price);

        out = _mintOut(price, usdgIn, spreadBps);
        if (out == 0) revert ZeroAmount();
        if (out < minOut) revert Slippage(out, minOut);

        _checkRisk(id, p.totalSupply(), out, price);

        usdg.safeTransferFrom(msg.sender, address(this), usdgIn);
        p.mint(to, out);
        emit Minted(id, to, usdgIn, out, price);
        if (queued > 0) _payQueue(3);
    }

    /// @dev The unbacked risk cap. Shares held for delayed sales still count
    ///      (a cancel hands them back). The risk carried by the unbacked shares
    ///      is what their mints added, so minting at rising prices cannot
    ///      re-value cheap ones; once the desk hedges some (whichever they were),
    ///      what is left can be no riskier than every share at the lowest price
    ///      minted since the market was last fully backed. Both bounds hold
    ///      whichever shares were hedged, so the smaller does too.
    function _checkRisk(uint256 id, uint256 supply, uint256 out, uint64 price) internal {
        uint256 held = backed[id];
        uint256 r = unbackedRisk[id];
        uint256 risk;
        uint256 low = price;
        if (supply > held) {
            if (r >> 192 < low) low = r >> 192;
            uint256 bound = (supply - held) * (ONE - low);
            risk = uint192(r) < bound ? uint192(r) : bound;
        }
        risk += out * (ONE - price);
        uint256 cap = maxRisk(id);
        if (risk / ONE > cap) revert RiskCap(risk / ONE, cap);
        unbackedRisk[id] = (low << 192) | SafeCast.toUint192(risk);
    }

    /// @dev A settled market mints at the payout plus the buy spread. The USDG stays
    ///      here and is the backing, so there is no band and no unbacked cap.
    function _mintSettled(uint256 id, PToken p, uint256 usdgIn, uint256 minOut, address to, uint64 payout)
        internal
        returns (uint256 out)
    {
        if (halted) revert Halted();
        if (to == address(0)) revert BadParams();
        if (payout == 0) revert SettledAtZero(id);
        _markHour();
        out = _mintOut(payout, usdgIn, buySpreadBps);
        if (out == 0) revert ZeroAmount();
        if (out < minOut) revert Slippage(out, minOut);
        usdg.safeTransferFrom(msg.sender, address(this), usdgIn);
        p.mint(to, out);
        emit Minted(id, to, usdgIn, out, payout);
        if (queued > 0) _payQueue(3);
    }

    function _redeem(uint256 id, PToken p, uint256 amountIn, uint256 minOut, address to, uint64 price, uint16 spreadBps)
        internal
        returns (uint256 out)
    {
        if (halted) revert Halted();
        if (to == address(0)) revert BadParams();
        _markHour();
        out = _redeemOut(price, amountIn, spreadBps);
        if (out < minOut) revert Slippage(out, minOut);
        emit Redeemed(id, to, amountIn, out, price);
        if (out == 0) {
            p.burn(msg.sender, amountIn);
            _unback(id, amountIn);
            return 0;
        }
        uint256 remaining = outflowRemaining();
        uint256 now_ = out <= remaining ? out : remaining;
        outflowInHour[block.timestamp / 3_600] += now_;
        // The part over the cap waits: its pToken is held here, its price fixed.
        uint256 pLater = now_ < out ? (amountIn * (out - now_)) / out : 0;
        // Dust tickets would only flood the release scan and the alert.
        if (pLater > 0 && out - now_ < MIN_DELAYED) revert DustOverCap(out - now_);
        p.burn(msg.sender, amountIn);
        _unback(id, amountIn - pLater);
        if (pLater > 0) {
            p.mint(address(this), pLater);
            delayedShares[id] += pLater;
            uint256 readyAt = block.timestamp + DELAY;
            delayed.push(
                DelayedSale(to, uint64(readyAt), id, SafeCast.toUint128(pLater), SafeCast.toUint128(out - now_))
            );
            emit SaleDelayed(delayed.length - 1, id, to, pLater, out - now_, readyAt);
        }
        if (now_ > 0) _pay(to, now_, false);
    }

    /// @dev Pay `amount` from the float now, queueing whatever it cannot cover.
    ///      With `setAside` (a release, which the seller did not time), a transfer
    ///      that fails for this recipient alone is kept for `withdrawUnclaimed`, as
    ///      the queue does; one that fails for everyone (USDG paused) reverts.
    function _pay(address to, uint256 amount, bool setAside) internal {
        if (queued > 0) _payQueue(3);
        uint256 free = freeFloat();
        uint256 now_ = amount <= free ? amount : free;
        if (now_ > 0 && !(setAside && _tryTransfer(to, now_))) {
            if (!setAside || !_tryTransfer(address(this), 0)) {
                usdg.safeTransfer(to, now_);
            } else {
                unclaimed[to] += now_;
                unclaimedTotal += now_;
                queued += now_;
            }
        }
        if (amount > now_) {
            uint256 rest = amount - now_;
            claims.push(Claim(to, SafeCast.toUint96(rest)));
            queued += rest;
            emit Queued(claims.length - 1, to, rest);
        }
    }

    function _payQueue(uint256 max) internal returns (uint256 paid) {
        uint256 head = claimHead;
        uint256 end = claims.length;
        uint256 bal = usdg.balanceOf(address(this));
        // Set-aside claims keep their USDG; the queue is paid from the rest.
        bal = bal > unclaimedTotal ? bal - unclaimedTotal : 0;
        while (head < end && paid < max) {
            Claim memory c = claims[head];
            if (c.amount > bal) break;
            bool sent = _tryTransfer(c.to, c.amount);
            // Every transfer failing (USDG paused) is not this recipient's fault:
            // stop, and keep the queue in order for when it resumes.
            if (!sent && !_tryTransfer(address(this), 0)) break;
            delete claims[head];
            // Either way the amount is spoken for: paid out, or reserved for its owner.
            bal -= c.amount;
            if (sent) {
                queued -= c.amount;
                emit ClaimPaid(head, c.to, c.amount);
            } else {
                // Still owed and still counted in `queued`, but no longer in line.
                unclaimed[c.to] += c.amount;
                unclaimedTotal += c.amount;
                emit ClaimUndelivered(head, c.to, c.amount);
            }
            ++head;
            ++paid;
        }
        claimHead = head;
    }

    /// @dev A USDG transfer that reports failure instead of reverting.
    function _tryTransfer(address to, uint256 amount) internal returns (bool) {
        (bool ok, bytes memory ret) = address(usdg).call(abi.encodeCall(IERC20.transfer, (to, amount)));
        return ok && (ret.length == 0 ? address(usdg).code.length > 0 : abi.decode(ret, (bool)));
    }

    function _mintOut(uint64 price, uint256 usdgIn, uint16 spreadBps) internal pure returns (uint256) {
        return (usdgIn * ONE) / _mulDivUp(price, BPS + spreadBps, BPS);
    }

    function _redeemOut(uint64 price, uint256 amountIn, uint16 spreadBps) internal pure returns (uint256) {
        return (amountIn * ((uint256(price) * (BPS - spreadBps)) / BPS)) / ONE;
    }

    function _market(address pToken) internal view returns (uint256 id, PToken p) {
        id = positionIdOf[pToken];
        p = PToken(pToken);
        if (id == 0 || address(pTokenOf[id]) != pToken) revert UnknownPToken(pToken);
    }

    function _quoted(uint256 id, uint256 amount, PriceOracle.Quote calldata q, bytes calldata sig, uint8 side)
        internal
        returns (uint64 price)
    {
        if (q.positionId != id) revert QuoteForOtherMarket(q.positionId, id);
        bytes32 digest;
        (price, digest) = oracle.verifyWithDigest(q, sig, side);
        // A quote covers `maxAmount` in total, however many transactions use it:
        // replaying one (it binds no taker) cannot multiply its size.
        uint256 used = quoteFilled[digest] + amount;
        if (used > q.maxAmount) revert QuoteTooSmall(used, q.maxAmount);
        quoteFilled[digest] = used;
    }

    function _mulDivUp(uint256 a, uint256 b, uint256 d) internal pure returns (uint256) {
        return (a * b + d - 1) / d;
    }

    /// @dev Disabled: a contract without an owner could never be resumed or reconfigured.
    function renounceOwnership() public pure override {
        revert BadParams();
    }
}
