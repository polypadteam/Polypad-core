// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
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
 * buying. The keeper reports the held amount as `backed`, and minting stops for
 * a market once `totalSupply - backed` would pass `maxUnbacked`. That caps what
 * a missed fill or a stale price can cost, per market.
 *
 * ## Guards on a mint
 *
 * - a live BUY quote for this market, covering this size
 * - market not paused and not settled
 * - price inside [minPrice, maxPriceOf(market)] (5c..95c by default; the owner
 *   can lift one market's ceiling up to 99c)
 * - unbacked supply under the cap
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
 * ## Posted-price path
 *
 * `mintPosted` / `redeemPosted` trade at the oracle's posted midpoint instead of
 * a signed quote, so any contract can trade without our API. They pay a wider
 * spread (`postedSpreadBps`) and are capped per trade and per market per block;
 * size belongs on the signed path, which prices off the real book depth.
 *
 * ## Circuit breaker
 *
 * USDG leaving through redemptions is capped per hour (`outflowCapPerHour`), so a
 * leaked signing key or a pricing bug cannot empty the float in one go. The
 * keeper (or owner) can `halt` every mint and redeem at once; only the owner
 * lifts it.
 */
contract PExchange is Ownable2Step, ReentrancyGuard {
    using SafeERC20 for IERC20;

    uint256 public constant ONE = 1e6;
    uint256 public constant BPS = 10_000;

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
    uint64 public maxPrice = 950_000; // 95c
    /// @notice Unbacked shares allowed per market unless overridden, 6 decimals.
    uint256 public defaultMaxUnbacked = 2_000e6;

    mapping(uint256 positionId => PToken) public pTokenOf;
    mapping(address pToken => uint256) public positionIdOf;
    mapping(uint256 positionId => uint256) public backed;
    mapping(uint256 positionId => uint256) public maxUnbackedOverride;

    /// @notice Extra spread on posted-price trades, both sides.
    uint16 public postedSpreadBps = 150;
    /// @notice Largest posted-price trade, USDG (6 decimals).
    uint256 public postedMaxTrade = 500e6;
    /// @notice Most USDG traded on the posted path per market per block.
    uint256 public postedMaxPerBlock = 2_000e6;
    /// @notice Most USDG that may leave through redemptions per clock hour.
    uint256 public outflowCapPerHour = 25_000e6;
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
    /// @notice USDG owed to the queue.
    uint256 public queued;

    struct BlockUse {
        uint64 blockNumber;
        uint192 used;
    }

    mapping(uint256 positionId => BlockUse) internal postedUse;
    mapping(uint256 hour => uint256) public outflowInHour;

    event PTokenCreated(uint256 indexed positionId, address pToken);
    event Minted(uint256 indexed positionId, address indexed to, uint256 usdgIn, uint256 pOut, uint256 price);
    event Redeemed(uint256 indexed positionId, address indexed to, uint256 pIn, uint256 usdgOut, uint256 price);
    event Absorbed(uint256 indexed positionId, uint256 amount);
    event BackedReported(uint256 indexed positionId, uint256 amount);
    event SentToBridge(address indexed to, uint256 amount);
    event ParamsSet(
        uint16 buySpreadBps, uint16 sellSpreadBps, uint64 minPrice, uint64 maxPrice, uint256 defaultMaxUnbacked
    );
    event RolesSet(address keeper, address factory, address bridgeDeposit);
    event BridgeDepositProposed(address indexed bridgeDeposit, uint256 effectiveAt);
    event Rescued(address indexed token, address indexed to, uint256 amount);
    event PostedParamsSet(uint16 postedSpreadBps, uint256 postedMaxTrade, uint256 postedMaxPerBlock);
    event OutflowCapSet(uint256 outflowCapPerHour);
    event HaltSet(bool halted);
    event Queued(uint256 indexed ticket, address indexed to, uint256 amount);
    event ClaimPaid(uint256 indexed ticket, address indexed to, uint256 amount);
    event SettleFeeSet(uint16 settleFeeBps);
    event MaxPriceSet(uint256 indexed positionId, uint64 maxPrice);
    event ShareLabelSet(uint256 indexed positionId, string name, string symbol);

    error OnlyKeeper();
    error OnlyFactory();
    error UnknownPToken(address pToken);
    error QuoteForOtherMarket(uint256 quoted, uint256 positionId);
    error QuoteTooSmall(uint256 amount, uint256 maxAmount);
    error PriceOutOfBand(uint256 price);
    error UnbackedCap(uint256 unbacked, uint256 cap);
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
    error PostedTradeTooLarge(uint256 amount, uint256 max);
    error PostedBlockCap(uint256 used, uint256 max);
    error OutflowCap(uint256 wanted, uint256 remaining);
    error SettledAtZero(uint256 positionId);

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
     * @notice `mint` at the oracle's posted price plus `postedSpreadBps`, no quote
     *         needed. At most `postedMaxTrade` USDG, and `postedMaxPerBlock` per
     *         market per block.
     */
    function mintPosted(address pToken, uint256 usdgIn, uint256 minOut, address to)
        external
        nonReentrant
        returns (uint256 out)
    {
        if (usdgIn == 0) revert ZeroAmount();
        (uint256 id, PToken p) = _market(pToken);
        (, bool settled, uint64 payout) = oracle.status(id);
        if (settled) return _mintSettled(id, p, usdgIn, minOut, to, payout);
        uint64 price = oracle.postedPrice(id, oracle.BUY());
        _usePosted(id, usdgIn);
        out = _mint(id, p, usdgIn, minOut, to, price, postedSpreadBps);
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
     * @notice `redeem` at the posted price less `postedSpreadBps`, or at the
     *         payout once settled. Posted redemptions share the posted size caps.
     */
    function redeemPosted(address pToken, uint256 amountIn, uint256 minOut, address to)
        external
        nonReentrant
        returns (uint256 out)
    {
        if (amountIn == 0) revert ZeroAmount();
        (uint256 id, PToken p) = _market(pToken);
        (, bool settled, uint64 payout) = oracle.status(id);
        if (settled) return _redeem(id, p, amountIn, minOut, to, payout, settleFeeBps);
        uint64 price = oracle.postedPrice(id, oracle.SELL());
        out = _redeem(id, p, amountIn, minOut, to, price, postedSpreadBps);
        _usePosted(id, out);
    }

    /**
     * @notice Burn pToken this contract holds. Curves send the float share of
     *         their fees here; burning it lowers supply, the desk sells the
     *         matching shares, and the proceeds land in the Polymarket balance.
     *         The USDG those shares were bought with already sits in the float.
     */
    function absorb(address pToken) external returns (uint256 amount) {
        (uint256 id, PToken p) = _market(pToken);
        amount = p.balanceOf(address(this));
        if (amount == 0) return 0;
        p.burn(address(this), amount);
        emit Absorbed(id, amount);
    }

    /**
     * @notice Pay up to `max` queued claims, oldest first, while the float covers
     *         them. Anyone may call.
     */
    function payQueue(uint256 max) external nonReentrant returns (uint256 paid) {
        return _payQueue(max);
    }

    /// @notice Claims still waiting.
    function queueLength() external view returns (uint256) {
        return claims.length - claimHead;
    }

    /* -------------------------------------------------------------- keeper */

    /// @notice Shares the desk holds on Polymarket, per market.
    function reportBacked(uint256[] calldata ids, uint256[] calldata amounts) external onlyKeeper {
        if (ids.length != amounts.length) revert BadParams();
        for (uint256 i; i < ids.length; ++i) {
            backed[ids[i]] = amounts[i];
            emit BackedReported(ids[i], amounts[i]);
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
            if (bridgeDeposit != address(0)) revert BridgeChangeDelayed();
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
        pendingBridgeDeposit = address(0);
        pendingBridgeDepositAt = 0;
        emit RolesSet(keeper, factory, bridgeDeposit);
    }

    function setParams(
        uint16 buySpreadBps_,
        uint16 sellSpreadBps_,
        uint64 minPrice_,
        uint64 maxPrice_,
        uint256 defaultMaxUnbacked_
    ) external onlyOwner {
        if (buySpreadBps_ > 500 || sellSpreadBps_ > 500 || minPrice_ >= maxPrice_ || maxPrice_ >= ONE) {
            revert BadParams();
        }
        buySpreadBps = buySpreadBps_;
        sellSpreadBps = sellSpreadBps_;
        minPrice = minPrice_;
        maxPrice = maxPrice_;
        defaultMaxUnbacked = defaultMaxUnbacked_;
        emit ParamsSet(buySpreadBps_, sellSpreadBps_, minPrice_, maxPrice_, defaultMaxUnbacked_);
    }

    function setMaxUnbacked(uint256 positionId, uint256 cap) external onlyOwner {
        maxUnbackedOverride[positionId] = cap;
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

    function setPostedParams(uint16 postedSpreadBps_, uint256 postedMaxTrade_, uint256 postedMaxPerBlock_)
        external
        onlyOwner
    {
        if (postedSpreadBps_ > 1_000) revert BadParams();
        postedSpreadBps = postedSpreadBps_;
        postedMaxTrade = postedMaxTrade_;
        postedMaxPerBlock = postedMaxPerBlock_;
        emit PostedParamsSet(postedSpreadBps_, postedMaxTrade_, postedMaxPerBlock_);
    }

    function setOutflowCap(uint256 outflowCapPerHour_) external onlyOwner {
        outflowCapPerHour = outflowCapPerHour_;
        emit OutflowCapSet(outflowCapPerHour_);
    }

    /// @notice Stop every mint and redeem. The keeper may halt; only the owner resumes.
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

    function maxUnbacked(uint256 positionId) public view returns (uint256) {
        uint256 o = maxUnbackedOverride[positionId];
        return o == 0 ? defaultMaxUnbacked : o;
    }

    /// @notice pToken minted for `usdgIn` at a quoted buy price.
    function mintOut(uint64 price, uint256 usdgIn) public view returns (uint256) {
        return _mintOut(price, usdgIn, buySpreadBps);
    }

    /// @notice USDG paid for `amountIn` pToken at a quoted sell price.
    function redeemOut(uint64 price, uint256 amountIn) public view returns (uint256) {
        return _redeemOut(price, amountIn, sellSpreadBps);
    }

    /// @notice pToken minted for `usdgIn` on the posted path, at the current posted price.
    function mintPostedOut(uint256 positionId, uint256 usdgIn) external view returns (uint256) {
        return _mintOut(oracle.postedPrice(positionId, oracle.BUY()), usdgIn, postedSpreadBps);
    }

    /// @notice USDG paid for `amountIn` pToken on the posted path, or at the payout once settled.
    function redeemPostedOut(uint256 positionId, uint256 amountIn) external view returns (uint256) {
        (, bool settled, uint64 payout) = oracle.status(positionId);
        if (settled) return _redeemOut(payout, amountIn, settleFeeBps);
        return _redeemOut(oracle.postedPrice(positionId, oracle.SELL()), amountIn, postedSpreadBps);
    }

    /// @notice USDG that may still leave through redemptions this hour.
    function outflowRemaining() public view returns (uint256) {
        uint256 used = outflowInHour[block.timestamp / 3_600];
        return used >= outflowCapPerHour ? 0 : outflowCapPerHour - used;
    }

    /* ------------------------------------------------------------ internal */

    function _mint(uint256 id, PToken p, uint256 usdgIn, uint256 minOut, address to, uint64 price, uint16 spreadBps)
        internal
        returns (uint256 out)
    {
        if (halted) revert Halted();
        if (price < minPrice || price > maxPriceOf(id)) revert PriceOutOfBand(price);

        out = _mintOut(price, usdgIn, spreadBps);
        if (out == 0) revert ZeroAmount();
        if (out < minOut) revert Slippage(out, minOut);

        uint256 supplyAfter = p.totalSupply() + out;
        uint256 held = backed[id];
        if (supplyAfter > held) {
            uint256 unbacked = supplyAfter - held;
            uint256 cap = maxUnbacked(id);
            if (unbacked > cap) revert UnbackedCap(unbacked, cap);
        }

        usdg.safeTransferFrom(msg.sender, address(this), usdgIn);
        p.mint(to, out);
        emit Minted(id, to, usdgIn, out, price);
        if (queued > 0) _payQueue(3);
    }

    /// @dev A settled market mints at the payout plus the buy spread. The USDG stays
    ///      here and is the backing, so there is no band and no unbacked cap.
    function _mintSettled(uint256 id, PToken p, uint256 usdgIn, uint256 minOut, address to, uint64 payout)
        internal
        returns (uint256 out)
    {
        if (halted) revert Halted();
        if (payout == 0) revert SettledAtZero(id);
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
        out = _redeemOut(price, amountIn, spreadBps);
        if (out < minOut) revert Slippage(out, minOut);
        uint256 remaining = outflowRemaining();
        if (out > remaining) revert OutflowCap(out, remaining);
        outflowInHour[block.timestamp / 3_600] += out;

        p.burn(msg.sender, amountIn);
        emit Redeemed(id, to, amountIn, out, price);
        if (out == 0) return 0;
        if (queued > 0) _payQueue(3);
        // Pay what the float can now; anything it cannot is owed, paid in turn.
        uint256 free = freeFloat();
        uint256 now_ = out <= free ? out : free;
        if (now_ > 0) usdg.safeTransfer(to, now_);
        if (out > now_) {
            uint256 rest = out - now_;
            claims.push(Claim(to, uint96(rest)));
            queued += rest;
            emit Queued(claims.length - 1, to, rest);
        }
    }

    function _payQueue(uint256 max) internal returns (uint256 paid) {
        uint256 head = claimHead;
        uint256 end = claims.length;
        uint256 bal = usdg.balanceOf(address(this));
        while (head < end && paid < max) {
            Claim memory c = claims[head];
            if (c.amount > bal) break;
            delete claims[head];
            bal -= c.amount;
            queued -= c.amount;
            usdg.safeTransfer(c.to, c.amount);
            emit ClaimPaid(head, c.to, c.amount);
            ++head;
            ++paid;
        }
        claimHead = head;
    }

    /// @notice Count `usdg` against the posted path's per-trade and per-block caps.
    function _usePosted(uint256 id, uint256 usdgAmount) internal {
        if (usdgAmount > postedMaxTrade) revert PostedTradeTooLarge(usdgAmount, postedMaxTrade);
        BlockUse storage u = postedUse[id];
        uint256 used = u.blockNumber == block.number ? u.used + usdgAmount : usdgAmount;
        if (used > postedMaxPerBlock) revert PostedBlockCap(used, postedMaxPerBlock);
        u.blockNumber = uint64(block.number);
        u.used = uint192(used);
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
        view
        returns (uint64)
    {
        if (q.positionId != id) revert QuoteForOtherMarket(q.positionId, id);
        if (amount > q.maxAmount) revert QuoteTooSmall(amount, q.maxAmount);
        return oracle.verify(q, sig, side);
    }

    function _mulDivUp(uint256 a, uint256 b, uint256 d) internal pure returns (uint256) {
        return (a * b + d - 1) / d;
    }
}
