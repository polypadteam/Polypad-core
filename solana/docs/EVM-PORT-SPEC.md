# Polypad EVM -> Solana (Anchor) porting spec

Source of truth: `contracts/src/*.sol` on branch `design-e` (v14, live block 87884024). Docs read:
`docs/SECURITY.md`, `docs/DESIGN-E.md`, `docs/FEES.md`.

**Where docs and code disagree, this spec follows the code.** Known stale doc values:
- DESIGN-E says the curve fee is 1% split 0.6/0.2/0.2 (creator/float/platform), the price band is 5-98c and
  `maxRisk` is $1,000. The code has: curve fee 1.4% (140 bps), split 50/50 creator/platform (no float share
  in the curve), band 0.1c-99.9c, `defaultMaxRisk` $5,000, `maxTotalRisk` $25,000.
- There is no 70/30 split anywhere in the code. The defaults are `Fees(140, 5_000, 10_000, 5_000)`, so both the
  curve fee and the pool fee go 50% to the creator side and 50% to the platform. The creator share is bounded to
  30%-80%. Within the creator side, `holdersBps` (0-100%) of it goes to holders.
- Deployed values: per memory and SECURITY.md ("caps off"), the live outflow cap and risk caps were raised to
  $1B or `type(uint256).max` by owner transactions after deploy. The defaults below are the constructor and
  initializer defaults.

Units used throughout:
- USDG: 6 decimals. pToken: 6 decimals (1e6 = 1 share). Prices: 6 decimals (`ONE = 1e6` = $1).
- Coin: **18 decimals, SUPPLY = 1e27**. That does not fit a u64 SPL amount (max ~1.8e19). See section 12.
- Risk: 12 decimals (shares x price).
- `BPS = 10_000`.

---

## 0. Contract map and call graph

```
LaunchFactory --launch--> PriceOracle.verify (BUY)
              --ensurePToken--> PExchange --CREATE2--> PToken
              --new--> BondingCurve, Coin (mints 1e27 to curve)
              --register--> FeeVault
              --initialize--> BondingCurve

Router.buy  --> PExchange.mint (--> PriceOracle.verifyWithDigest | status/settlement) --> PToken.mint
            --> BondingCurve.buy (--> FeeVault.deposit, platform transfer; maybe --> Graduator.graduate --> v4 PoolManager)
            --> (pool) PoolManager.unlock -> Router.unlockCallback -> swap
Router.sell --> BondingCurve.sell | pool swap --> PExchange.redeem --> PToken.burn, USDG pay or queue or delay
Coin._update --> FeeVault.onTransfer (only if holdersBps > 0)
Graduator.collect --> PoolManager.unlock -> modifyLiquidity(0) -> fees -> FeeVault.deposit + platform
FeeVault.withdrawUsd / claimFor(cashOut) --> PExchange.redeem
PriceFeed: standalone (display only)
```

Roles:
- **owner**: Ownable2Step (`transferOwnership` sets pending; `acceptOwnership` by pending). `renounceOwnership`
  is overridden to revert `BadParams()` on PriceOracle, PExchange and LaunchFactory. PriceFeed keeps the default
  OZ renounce.
- **signer** (oracle): an off-chain key whose ECDSA signatures over EIP-712 quotes are checked. It holds no funds and
  sends no transactions.
- **keeper** (oracle): pauses and settles markets. It is a separate address variable from the PExchange keeper, but
  in practice the same key holds both.
- **keeper** (exchange): reports backing, sends float to the bridge, halts, and sets share labels.
- **factory** (exchange): may call `ensurePToken`. In the factory, FeeVault and Graduator this is an immutable
  `factory` address.
- **poster** (PriceFeed).
- **payee** (FeeVault, per coin): the creator, transferable by the payee.

---

## 1. PriceOracle

**Purpose.** Verifies the pricer's signed, one-sided, short-lived quotes (EIP-712, secp256k1) and holds each
market's pause flag and its one-way settlement (payout per share), which takes effect after a delay.

### Constants
| name | value |
|---|---|
| ONE | 1e6 (uint64) |
| MAX_VALIDITY | 30 (seconds) |
| MAX_PRICE | 999_000 (99.9c) |
| BUY / SELL | 0 / 1 (uint8) |
| QUOTE_TYPEHASH | keccak256("Quote(uint256 positionId,uint8 side,uint64 price,uint256 maxAmount,uint64 validUntil)") |
| EIP712 domain | name "Polypad Oracle", version "1" (chainId + verifyingContract implicit) |
| ROLE_DELAY | 12 hours |
| ACCEPT_WINDOW | 7 days |
| SETTLE_DELAY | 1 hour |

### Types
`Quote { uint256 positionId; uint8 side; uint64 price; uint256 maxAmount; uint64 validUntil }`.
`maxAmount` is in USDG for a BUY and in pToken for a SELL.

`Status` (per positionId):
| field | type | default | writer |
|---|---|---|---|
| paused | bool | false | keeper (setPaused), settle, cancelSettle |
| pausedBeforeSettle | bool | false | settle, cancelSettle |
| payout | uint64 (6dp, 0..1e6) | 0 | settle, cancelSettle |
| settleAt | uint64 (unix ts; 0 = none) | 0 | settle, cancelSettle |
| settledBy | address | 0 | settle, cancelSettle |
| barred | address | 0 | cancelSettle |

### State
| var | type | default | writer |
|---|---|---|---|
| statuses | mapping(uint256 => Status) internal | - | see above |
| signer | address | ctor arg | setSigner(0), acceptSigner |
| pendingSigner | address | 0 | setSigner, acceptSigner |
| pendingSignerAt | uint256 ts | 0 | setSigner |
| keeper | address | ctor arg | setKeeper |
| owner / pendingOwner | Ownable2Step | ctor owner_ | |

### Functions
**constructor(owner_, signer_, keeper_)**: sets fields; emits `SignerSet(signer_)`, `KeeperSet(keeper_)`.

**setSigner(address signer_)**, onlyOwner:
- If `signer_ == 0`: `signer = 0`, `pendingSigner = 0`, emit `SignerSet(0)`, return. The revoke takes effect at once.
- Else: `pendingSigner = signer_`, `pendingSignerAt = now + ROLE_DELAY`, emit `SignerProposed(signer_, pendingSignerAt)`.

**acceptSigner()**, onlyOwner:
1. `pendingSigner == 0 || now > pendingSignerAt + ACCEPT_WINDOW` -> `NothingPending()`
2. `now < pendingSignerAt` -> `NotYet(pendingSignerAt)`
3. `signer = pendingSigner; pendingSigner = 0`; emit `SignerSet(signer)`. (`pendingSignerAt` is not cleared.)

**setKeeper(keeper_)**, onlyOwner. No checks (zero is allowed). Emits `KeeperSet`.

**setPaused(uint256[] ids, bool paused_)**, onlyKeeper (`OnlyKeeper()`): for each id, if `!paused_ && settleAt != 0`
it skips the id silently (a market with a pending or effective settlement never reopens; only `cancelSettle`
reopens it). Else it sets `paused = paused_` and emits `PausedSet(id, paused_)`.

**settle(uint256 id, uint64 payout)**, owner, or keeper not barred on this id:
1. `msg.sender != owner && (msg.sender != keeper || msg.sender == s.barred)` -> `OnlyKeeper()`
2. `payout > ONE` -> `PriceOutOfRange(id, payout)`
3. `s.settleAt != 0` -> `AlreadySettled(id)`. This also blocks re-settling while a settlement is pending.
4. `at = now + SETTLE_DELAY`; `pausedBeforeSettle = paused`; `paused = true`; `payout = payout`; `settleAt = at`;
   `settledBy = msg.sender`.
5. emit `PausedSet(id,true)`, `Settled(id, payout, at)`.

**cancelSettle(uint256 id)**, onlyOwner:
1. `settleAt == 0 || now >= settleAt` -> `NothingPending()`. Once in effect it is one-way.
2. `settleAt = 0; payout = 0`; if `settledBy != owner` then `barred = settledBy`; `settledBy = 0`.
3. `was = pausedBeforeSettle; pausedBeforeSettle = false`; if `!was` then `paused = false` and emit `PausedSet(id,false)`.
4. emit `SettleCancelled(id)`.

**settlement(id) view** -> `(payout, settleAt)`. It returns the raw values, including a pending settlement.

**status(id) view** -> `(paused, settled, settled ? payout : 0)`, where `settled = settleAt != 0 && now >= settleAt`.

**verify(q, sig, side) view** -> price. **verifyWithDigest(q, sig, side) view** -> (price, digest). `_verify` checks in
this order:
1. `q.side != side` -> `WrongSide(q.side)`
2. `now > q.validUntil` -> `QuoteExpired(validUntil)`
3. `q.validUntil > now + MAX_VALIDITY` -> `QuoteTooLong(validUntil)`
4. `q.price == 0 || q.price > MAX_PRICE` -> `PriceOutOfRange(positionId, price)`
5. `digest = _hashTypedDataV4(keccak256(abi.encode(TYPEHASH, positionId, side, price, maxAmount, validUntil)))`;
   `ECDSA.recover(digest, sig) != signer` -> `BadSignature()`. OZ ECDSA rejects malleable high-s signatures and bad
   lengths, which throw OZ errors. If `signer == 0`, recover never returns 0 (it reverts), so all quotes are refused.
6. If settled (effective) -> `MarketSettled(id)`.
7. `side == BUY && paused` -> `MarketPaused(id)`. A SELL is allowed while paused.

**quoteDigest(q) view public**.

**renounceOwnership()** -> reverts `BadParams()`.

Events: SignerSet, SignerProposed, KeeperSet, PausedSet, Settled, SettleCancelled.

### Invariants
- Settlement is one-way once `now >= settleAt`. `cancelSettle` works only strictly before it.
- A market with `settleAt != 0` cannot be unpaused by the keeper.
- One barred address per market (an accepted limitation).
- No quote price is ever 0 or above 99.9c.
- A quote's life is at most 30s from the block it lands in.

### Solana redesign
- **EIP-712 + ecrecover.** Option A (recommended): the signer is an ed25519 key. The client includes an
  `Ed25519Program` verify instruction in the same transaction. The program reads the Instructions sysvar
  (`load_instruction_at_checked`) and checks the pubkey, the message bytes and the signature offsets, and that the
  instruction indexes point to the same instruction. The message is a fixed Borsh layout: `domain_tag ("Polypad
  Oracle v1" || program_id || cluster/genesis tag) || position_id[32] || side u8 || price u64 || max_amount u64 ||
  valid_until i64`. Option B: keep secp256k1 with `secp256k1_recover` syscall, keccak and the same EIP-712 digest,
  so the existing pricer keeps working. Low-s must be enforced manually. The digest is either the sha256/keccak of the
  message or, for ed25519, the signature itself.
- **Per-market Status** -> PDA `["market", position_id(32B)]` holding `paused, paused_before_settle, payout u64,
  settle_at i64, settled_by Pubkey, barred Pubkey`. It is created by the factory or keeper on first use. The
  exchange's per-market state can live in the same account if one program holds both, which is recommended.
- **Config** -> PDA `["oracle"]`: `owner, pending_owner, signer, pending_signer, pending_signer_at, keeper`.
- `block.timestamp` -> `Clock::get()?.unix_timestamp` (i64; note that it can drift).
- `setPaused(ids[])`: pass market PDAs in `remaining_accounts` and check each PDA's seeds.
- Cross-program view calls (`status`, `settlement`) become plain account reads if the oracle and exchange are one
  program. If they are separate, pass the market account and check its owner and seeds.

---

## 2. PToken

**Purpose.** One ERC-20 per Polymarket outcome (CTF positionId, which is also the CLOB token id). 1 token = 1 share,
6 decimals. Only the exchange that deployed it may mint or burn it, or set its label.

### State
| var | type | default | writer |
|---|---|---|---|
| positionId | uint256 immutable | ctor | - |
| exchange | address immutable | msg.sender at deploy | - |
| label / ticker | string | "" (name falls back to "Polypad Share", symbol to "pSHARE") | exchange via setLabel |
| ERC20 balances/supply | | | mint/burn/transfer |

### Functions
- `name()` / `symbol()`: return the label or ticker if set, else the defaults. `decimals()` = 6.
- `setLabel(name_, symbol_)`: only the exchange (`OnlyExchange()`). There are no length checks here; the exchange
  checks `name <= 64` and `symbol <= 16` bytes.
- `mint(to, amount)`: only the exchange.
- `burn(from, amount)`: only the exchange. It burns from `from` with no allowance check. The exchange only calls it
  with `from = msg.sender of redeem` or with its own address.

### Solana redesign
- An SPL mint (classic Token, or Token-2022 to get the metadata extension) at PDA `["ptoken", position_id]`, decimals
  6, with mint authority = the exchange PDA `["exchange"]`. There is no freeze authority, or it is set to None.
- CREATE2 address prediction (`pTokenAddress`) -> `Pubkey::find_program_address`.
- `setLabel` -> Token-2022 TokenMetadata `update_field` (name, symbol), or a Metaplex metadata update with the
  exchange PDA as update authority.
- **Burn semantics.** An SPL burn needs the token-account owner (or a delegate) to sign. In `redeem` the seller signs
  the instruction, so the program CPIs `burn` with the user as authority, which works without approval. For burns of
  exchange-held tokens (absorb, release), the exchange PDA owns the token account and signs with seeds.
- `positionIdOf[pToken]` reverse lookup -> unnecessary. Derive the PDA from the position id passed in and compare it
  with the mint account.

---

## 3. PExchange

**Purpose.** Holds one shared USDG float. It mints pToken for USDG at a signed BUY quote x (1 + spread), and burns
pToken for USDG at a signed SELL quote x (1 - spread), or at the payout after settlement. It tracks unbacked risk
per market and overall, meters redemption outflow with a sliding-hour cap (excess is delayed), and pays shortfalls
through a FIFO claim queue.

### Constants
| name | value |
|---|---|
| ONE | 1e6 |
| BPS | 10_000 |
| BRIDGE_DELAY | 2 days |
| DELAY (delayed sale) | 1 hour |
| MIN_DELAYED | 1e6 ($1) |
| hour bucket | 3_600 s |
| `_payQueue` batch inside mint/pay | 3 |

### State
| var | type / units | default | writer |
|---|---|---|---|
| usdg | IERC20 immutable | ctor | - |
| oracle | PriceOracle immutable | ctor | - |
| keeper | address | ctor | setRoles |
| factory | address | 0 | setRoles |
| bridgeDeposit | address | 0 | setRoles (first time only, or clear), acceptBridgeDeposit |
| pendingBridgeDeposit | address | 0 | propose/accept |
| pendingBridgeDepositAt | uint256 ts | 0 | propose/accept |
| bridgeEverSet | bool | false | setRoles, acceptBridgeDeposit |
| buySpreadBps | uint16 | 25 | setParams |
| sellSpreadBps | uint16 | 25 | setParams |
| minPrice | uint64 6dp | 1_000 (0.1c) | setParams |
| maxPrice | uint64 6dp | 999_000 (99.9c) | setParams |
| meterSales | bool | **true** (comment says off in deployment; Deploy env METER_SALES) | setMeterSales |
| defaultMaxRisk | uint256 USDG 6dp | 5_000e6 | setParams |
| maxTotalRisk | uint256 USDG 6dp | 25_000e6 | setMaxTotalRisk |
| totalRisk | uint256, **12dp** | 0 | _storeRisk |
| pTokenOf | mapping(id => PToken) | | ensurePToken |
| positionIdOf | mapping(pToken => id) | | ensurePToken |
| backed | mapping(id => uint256 shares 6dp) | 0 | _report, _burned |
| maxRiskOverride | mapping(id => USDG 6dp) | 0 (= use default) | setMaxRisk |
| outflowFloor | uint256 USDG | 5_000e6 | setOutflowCap |
| outflowFloatBps | uint256 | 5_000 (50%) | setOutflowCap |
| halted | bool | false | halt / resume |
| settleFeeBps | uint16 | 50 | setSettleFee |
| claims | Claim[] {address to; uint96 amount} | [] | _pay push, _payQueue delete |
| claimHead | uint256 | 0 | _payQueue |
| queued | uint256 USDG | 0 | _pay, _payQueue, withdrawUnclaimed |
| unclaimed | mapping(address => USDG) | 0 | _pay(setAside), _payQueue, withdrawUnclaimed |
| unclaimedTotal | uint256 | 0 | same |
| quoteFilled | mapping(bytes32 digest => uint256) | 0 | _quoted |
| outflowInHour | mapping(uint256 hour => USDG) | 0 | _redeem |
| hourFloat | HourFloat {uint64 hour; uint96 amount; uint96 base} | 0 | _markHour |
| delayed | DelayedSale[] {address to; uint64 readyAt; uint256 positionId; uint128 pAmount; uint128 amount} | [] | _redeem push, release/cancel delete |
| delayedShares | mapping(id => pToken 6dp) | 0 | _redeem, release, cancel |
| unbackedRisk | mapping(id => uint256 packed) | 0 | _storeRisk |
| mintedSinceReport | mapping(id => shares) | 0 | _checkRisk (+), _report (delete), _burned (-) |
| burnedTotal | mapping(id => shares) | 0 | _burned |

`unbackedRisk` packing: bits 0-127 = risk (12dp, `shares x (ONE - price)`), bits 128-191 = `low` (lowest mint price
since last fully backed; 0 = unknown, taken as $0), bits 192+ = `high` (highest; `ONE` = unknown, taken as $1).

### Pure math
- `_mintOut(price, usdgIn, spread) = floor(usdgIn * ONE / ceil(price * (BPS + spread) / BPS))`. The effective price is
  rounded **up**, then output is rounded **down**.
- `_redeemOut(price, amountIn, spread) = floor(amountIn * floor(price * (BPS - spread) / BPS) / ONE)`. The effective
  price is rounded down, then output is rounded down.
- `_mulDivUp(a,b,d) = (a*b + d - 1)/d`.
- `freeFloat() = max(0, usdg.balanceOf(this) - queued)`.
- `maxRisk(id) = override != 0 ? override : defaultMaxRisk`.
- `_bounded(risk, supply, held, low) = supply <= held ? 0 : min(risk, (supply - held) * (ONE - low))`.
- `_storeRisk(id, risk, low, high)`: `totalRisk = totalRisk + risk - uint128(old)`, then store packed (SafeCast u128).
- `_base(f, cur) = (f.hour == 0 || cur < f.amount) ? cur : f.amount`, i.e. min(cur, last snapshot's float) when a
  snapshot exists.
- `_sliding(m, hour, into) = m[hour] + floor(m[hour-1] * (3600 - into) / 3600)` (with `hour == 0` treated as prev 0).
- `outflowRemaining()`: `hour = now/3600; into = now%3600; used = _sliding(...)`;
  `cap = floor((f.hour == hour ? f.base : _base(f, freeFloat())) * outflowFloatBps / BPS)`; `cap = max(cap,
  outflowFloor)`; returns `used >= cap ? 0 : cap - used`.

### Functions

**constructor(owner_, usdg_, oracle_, keeper_)**.

**ensurePToken(positionId)** -> PToken. Caller is the factory or owner (`OnlyFactory()`). If the pToken exists it
returns it. Else it deploys `new PToken{salt: bytes32(positionId)}(positionId)`, records both maps and emits
`PTokenCreated`. There is no check that `positionId != 0`, but `_market` rejects id 0 (`positionIdOf == 0` ->
unknown), so a positionId 0 market can never trade.

**pTokenAddress(positionId) view**: the CREATE2 prediction.

**mint(pToken, usdgIn, minOut, to, q, sig)**, nonReentrant, anyone:
1. `usdgIn == 0` -> `ZeroAmount()`
2. `_market(pToken)`: `id = positionIdOf[pToken]`; `id == 0 || pTokenOf[id] != pToken` -> `UnknownPToken`
3. `_payoutMint(id)`: if `oracle.status(id).settled` -> (atPayout, final=true, payout). Else if
   `oracle.settlement(id)` has `settleAt != 0 && payout == ONE` -> (atPayout, final=false, ONE), i.e. **a pending $1
   settlement mints at $1 immediately**. Otherwise it is a normal mint.
4. If atPayout -> `_mintSettled` (quote **ignored entirely**, not verified, not filled).
5. Else `price = _quoted(id, usdgIn, q, sig, BUY)`:
   - `q.positionId != id` -> `QuoteForOtherMarket`
   - `oracle.verifyWithDigest` (all oracle errors above; BUY also needs unpaused)
   - `used = quoteFilled[digest] + usdgIn; used > q.maxAmount` -> `QuoteTooSmall(used, maxAmount)`;
     `quoteFilled[digest] = used`
6. `_mint(id, p, usdgIn, minOut, to, price, buySpreadBps)`:
   1. `halted` -> `Halted()`
   2. `to == 0` -> `BadParams()`
   3. `_markHour()` (snapshot of free float at the first trade of the clock hour, **before** this trade's USDG arrives)
   4. `price < minPrice || price > maxPrice` -> `PriceOutOfBand(price)`
   5. `out = _mintOut(price, usdgIn, spread)`; `out == 0` -> `ZeroAmount()`; `out < minOut` -> `Slippage(out, minOut)`
   6. `_checkRisk(id, p.totalSupply() /*pre-mint*/, out, price)`, below
   7. `usdg.safeTransferFrom(msg.sender, this, usdgIn)`; `p.mint(to, out)`
   8. emit `Minted(id, to, usdgIn, out, price)`
   9. if `queued > 0` -> `_payQueue(3)`

**_checkRisk(id, supply, out, price)**:
```
held = backed[id]; (risk, low, high) = unpack(unbackedRisk[id])
if supply <= held:          (risk, low, high) = (0, price, price)
else:
   if high == ONE:          risk = (supply - held) * ONE        // unknown -> whole gap at $1
   low  = min(low, price); high = max(high, price)
   risk = _bounded(risk, supply, held, low)
risk += out * (ONE - price)
cap = maxRisk(id); if risk/ONE > cap -> RiskCap(risk/ONE, cap)
_storeRisk(id, risk, low, high)
mintedSinceReport[id] += out
if totalRisk/ONE > maxTotalRisk -> RiskCap(totalRisk/ONE, maxTotalRisk)
```
Note: `supply` includes `delayedShares` (held by the exchange), so delayed-sale shares count as unbacked until they
are released. This is deliberate (SECURITY v9 fix).

**_mintSettled(id, p, usdgIn, minOut, to, payout, final_)**:
1. `halted` -> `Halted()`; `to == 0` -> `BadParams()`; `payout == 0` -> `SettledAtZero(id)`
2. `_markHour()`
3. `out = _mintOut(payout, usdgIn, buySpreadBps)`; zero -> `ZeroAmount`; `< minOut` -> `Slippage`
4. Pull USDG, mint. If `final_`, `_storeRisk(id, 0, 0, 0)`. A pending $1 settlement keeps the stored risk in case it
   is cancelled.
5. emit `Minted(id, to, usdgIn, out, payout)`; if `queued > 0`, `_payQueue(3)`.
- No price band, no risk cap, no `mintedSinceReport`. The USDG stays in the float as backing.

**redeem(pToken, amountIn, minOut, to, q, sig)**, nonReentrant, anyone:
1. `amountIn == 0` -> `ZeroAmount`
2. `_market` -> `UnknownPToken`
3. `(, settled, payout) = oracle.status(id)`; `price = settled ? payout : _quoted(id, amountIn, q, sig, SELL)`. A SELL
   works while paused. A **pending** settlement does not change redemption: sells still need a quote, and the oracle
   allows sells until settleAt.
4. `_redeem(id, p, amountIn, minOut, to, price, settled)`:
   1. `halted` -> `Halted`; `to == 0` -> `BadParams`
   2. `_markHour()`
   3. `out = _redeemOut(price, amountIn, settled ? settleFeeBps : sellSpreadBps)`; `out < minOut` -> `Slippage`
   4. emit `Redeemed(id, to, amountIn, out, price)` (emitted **before** the burn and pay)
   5. If `out == 0`: `p.burn(msg.sender, amountIn)`; `_burned(id, amountIn)`; return 0. This happens with a $0 payout
      or dust.
   6. `metered = settled || meterSales`; `remaining = metered ? outflowRemaining() : out`; `now_ = min(out, remaining)`
   7. If `out - now_ < MIN_DELAYED`: if `now_ == 0` -> `DustOverCap(out)`; else `now_ = out`. A sub-$1 excess is paid
      now if any cap was left.
   8. If metered: `outflowInHour[now/3600] += now_`
   9. `pLater = now_ < out ? floor(amountIn * (out - now_) / out) : 0`
   10. `p.burn(msg.sender, amountIn)`
   11. If `pLater > 0`: `p.mint(this, pLater)`; `delayedShares[id] += pLater`; push `DelayedSale(to, now + DELAY, id,
       u128(pLater), u128(out - now_))`; emit `SaleDelayed(ticket = len-1, id, to, pLater, out - now_, readyAt)`
   12. `_burned(id, amountIn - pLater)`
   13. If `now_ > 0`: `_pay(to, now_, setAside=false)`

**_burned(id, n)**. `n` shares have already left the supply.
```
burnedTotal[id] += n
b = backed[id]; supply = totalSupply (post-burn)
k = min(mintedSinceReport[id], n, supply + n - b)      // the pre-burn unbacked gap
mintedSinceReport[id] -= k
b = (b > n - k) ? b - (n - k) : 0 ; backed[id] = b      // the rest were backed shares: backing drops
(risk, low, high) = unpack
if risk == 0: return
if oracle settled: _storeRisk(id, 0,0,0); return
off = k * (ONE - high)
_storeRisk(id, _bounded(risk > off ? risk - off : 0, supply, b, low), low, high)
```

**absorb(pToken)**, anyone: `_market`; `amount = p.balanceOf(this) - delayedShares[id]`; if 0 return 0; burn from this;
`_burned`; emit `Absorbed`. It never touches delayed-sale stock.

**payQueue(max)**, nonReentrant, anyone: `halted` -> `Halted()`; returns `_payQueue(max)`.

**_payQueue(max)**:
```
head = claimHead; end = claims.length
bal = max(0, usdg.balanceOf(this) - unclaimedTotal)       // set-aside USDG is never paid to the queue
while head < end && paid < max:
   c = claims[head]
   if c.amount > bal: break                                  // strict FIFO: no skipping a big claim
   sent = _tryTransfer(c.to, c.amount)
   if !sent && !_tryTransfer(this, 0): break                 // USDG globally failing (paused): stop, keep order
   delete claims[head]; bal -= c.amount
   if sent: queued -= c.amount; emit ClaimPaid(head, to, amt)
   else:    unclaimed[to] += amt; unclaimedTotal += amt; emit ClaimUndelivered(head, to, amt)   // still in `queued`
   ++head; ++paid
claimHead = head
```
`_tryTransfer` is a low-level `transfer` call returning ok and (empty return data with code at the token, or
decoded true).

**_pay(to, amount, setAside)**:
```
if queued > 0: _payQueue(3)                // older claims first
free = freeFloat(); now_ = min(amount, free)
if now_ > 0 and !(setAside && _tryTransfer(to, now_)):
    if !setAside || !_tryTransfer(this, 0): usdg.safeTransfer(to, now_)   // direct sale: revert if it fails; release + global failure: revert
    else: unclaimed[to] += now_; unclaimedTotal += now_; queued += now_    // release to a frozen recipient: set aside
if amount > now_: rest = amount - now_; claims.push(Claim(to, u96(rest))); queued += rest; emit Queued(len-1, to, rest)
```

**withdrawUnclaimed(to)**, nonReentrant, anyone: `amount = unclaimed[to]`; 0 -> `ZeroAmount`; zero the entry;
`unclaimedTotal -= amount`; `queued -= amount`; `safeTransfer(to, amount)` (pays only `to`; reverts while it is
frozen); emit `UnclaimedWithdrawn(to, to, amount)`. It is not blocked by `halted`.

**queueLength() view** = `claims.length - claimHead`.

**release(ticket)**, nonReentrant, anyone:
1. `halted` -> `Halted`
2. `d = delayed[ticket]` (an out-of-bounds index panics); `d.to == 0` -> `NothingPending`
3. `now < d.readyAt` -> `NotYet(readyAt)`
4. delete the ticket; `delayedShares[pid] -= pAmount`; burn `pAmount` from this; `_burned(pid, pAmount)`; emit
   `Released(ticket, pid)`
5. `_pay(d.to, d.amount, setAside=true)`. The delayed part is **never** added to `outflowInHour`: only `now_` was
   metered at sale time, and `release` does not meter. Its price stays fixed from the sale.

**delayedLength() view**.

**cancel(ticket)**, onlyOwner: `d.to == 0` -> `NothingPending`; delete; `delayedShares -= pAmount`;
`safeTransfer pToken (pAmount) to d.to`; emit `Cancelled(ticket)`. The shares go back to the payee and the USDG is
never paid. It works while halted and has no time check.

**reportBacked(ids[], amounts[], seen[])**, onlyKeeper: lengths must match (`BadParams`); `_report` for each.

**_report(id, amount, seen)**:
```
if seen > burnedTotal[id]: return              // skipped, does not revert the batch
supply = pToken ? totalSupply : 0
was = backed[id]
lost = was > amount ? was - amount : 0          // computed BEFORE subtracting `gone`
gone = burnedTotal[id] - seen
amount = amount > gone ? amount - gone : 0
amount = min(amount, supply)
(risk, low, high) = unpack
backed[id] = amount; delete mintedSinceReport[id]
(paused, settled) = oracle.status(id)
if settled:            risk = 0               (low/high kept as is)
elif lost > 0:         (risk, low, high) = (paused ? risk : risk + lost*ONE, 0, ONE)   // mark unknown
elif amount > was:     off = (amount - was) * (ONE - high); risk = _bounded(max(0, risk - off), supply, amount, low)
_storeRisk(id, risk, low, high); emit BackedReported(id, amount)
```

**Known v14 gap in `_report`; fixed on Solana only (deliberate divergence).** v14 takes every share burned since the
desk's read off what it reports (`gone`), as shares the desk may have sold since. The trouble is shares minted
after the read and burned before the report. Their `_burned` took them out of `mintedSinceReport` and took
`(ONE - high)` each off as unhedged shares leaving, and the desk never held or sold anything for them. The report
then also takes them off backing, so the gap grows by them but carries no risk for them. A later report that hedges
the gap takes `(ONE - high)` a share off again, and that comes out of the risk real unhedged shares carry. The
Solana fuzz found this (seed 100, 30x600): 566 shares minted and burned between a read and its report, then a
near-full hedge report. Risk ended at 0 with 3.77 quoted shares still unhedged; the most it could understate there
was 566 x 0.244, about $138. The same sequence works on Robinhood v14.

A first fix (Phase 6) put `(ONE - high)` a share back on the risk whenever a report lowered backing by burns
alone. That closed the understatement but reopened the round-trip griefing of SECURITY.md "Fourth review of v12":
mint-and-redeem round trips between a read and its report then lowered backing and raised stored risk until the
next report (the Phase 6 review's Probe C: 50 round trips of 100 shares took risk from $50 to $500). Nor could
`minted_total` at the read alone fix it: totals since the read cannot tell a share minted and then burned (the desk
sold nothing) from a backed share burned before a new one was minted (the desk may have sold one at the dip and not
bought the new one yet).

What does tell them apart is the lowest the supply went. The desk sells only shares beyond the supply
(`hedgeTarget` never targets below what it holds while that is within the supply), so after reading `amount` it
still holds at least `min(amount, lowest supply since the read)`. (Unwinding a decided market sells below the
supply; v14's `gone` did not cover that either, and the next report shows it as lost backing on a market that is
paused or settled.) And whatever was backed it still holds: it held
that at the last report, and has sold since only down to a supply that `backed` never exceeds. So `polypad_exchange`
(`math.rs` `Config::report`) keeps two more fields per market and takes `minted`, the market's `minted_total` from
the same read as `seen`:

- `low_supply`: the lowest supply seen since the last report. Set to the supply at each report, lowered at every
  instruction that reads the supply (`burned_outside`, before any mint can lift it, so a direct burn between
  instructions shows too) and after every exchange burn.
- `burned_at_report`: `burned_total` at the last report.

```
bookDirectBurns(id, supply)                                  // Solana only (`burned_outside`); see below
if seen > burnedTotal || minted > mintedTotal || seen > minted: return      // skipped, as before
was = backed; lost = was > amount ? was - amount : 0                         // unchanged
least = max(sat(minted - burnedTotal),                       // every burn since the read before any mint since
            min(lowSupply, sat(minted - burnedAtReport)))    // read after the last report: the low since it;
                                                             // before it: as low as the burns up to it could go
amount = lost == 0 ? min(amount, max(least, was)) : min(amount, least)
amount = min(amount, supply)                                 // redundant defence: least and was are within it
backed = amount; delete mintedSinceReport; lowSupply = supply; burnedAtReport = burnedTotal
settled / lost > 0 / amount > was: as above
```

`seen` is only a consistency check now: a read whose `seen` is ahead of the market's burns (or above its `minted`)
is skipped, and otherwise `seen` never enters the bound, which uses `minted` and the market's own counts. A port must
not reintroduce it there. In particular `burnedAtReport` must be the market's `burnedTotal` at the report, never the
report's `seen`: a stale read reported once (held to the dip since it) and then again, after the supply recovered,
would otherwise be bounded by `minted - seen`, the supply at the read, and credited the shares the desk sold at the
dip. (Back 100 of 100 at 50c, read, redeem 33, mint 50, report 100 twice: backing 67 both times, risk 50 x 0.5;
with `seen` it is 100 the second time.)

**A skipped report changes nothing** but the booking of direct burns, and that booking goes into the market and
the global total alike, so both must be stored, skipped or not. On Solana `report` books direct burns first (the skip
check compares against counts with them included) and `report_backed`, like every batch instruction, loads each
market through `with_market`, which always writes the market back; the config is written at the end of the
instruction. Writing the market only for applied reports (as the first version of this design did) let every skip
take the same burn's risk off `total_risk` once more while the market kept it: one batch with the same market eight
times took the total from $399 to $39 with the market still at $359, past `max_total_risk`. A market passed twice in
one batch is one account, so each entry sees what the one before stored. On the EVM there are no direct burns, so
the skip is a plain `return` before any write, and `_storeRisk` keeps the total the sum whatever.

With nothing lost the report never lowers backing, so the `amount < was` arm is gone: no risk is ever put back.
A share minted after the read and burned before the report changes nothing (its mint added its risk, its burn took
it off, the supply never fell for it), and round trips between a read and its report neither lower backing nor
raise risk, while a hedge from the same read is still credited in full. Burns that took the supply below what the
desk read hold the report to that dip (the desk may have sold there). `lost` is as before: only the desk holding
less than was backed, as it read it, is lost backing. A read taken before the last report and landing after it is
bounded by the burns up to that report too; with nothing lost it still never lowers backing.

The same holds for direct SPL burns: `burned_outside` takes `k x (ONE - high)` off for the `k` shares it counts
against `minted_since_report`, as `_burned` does for the same shares redeemed (see the redesign list below), never
`(ONE - low)`: `high` is the highest price minted since the market was last fully backed, so `(ONE - high)` is the
least any of those shares carried. The books after a direct burn and after a redemption of the same shares are the
same, and the report is exact for both. (Mint 100 at 30c and 100 at 60c, burn 10: risk falls by 10 x 0.4.)

**The equivalent EVM change** (not applied; Solidity is unchanged at v14). The EVM has no direct burns and no
`mintedTotal`, and `totalSupply + burnedTotal` is the minted total at any moment, so the keeper passes the supply
it read instead (`reportBacked(ids, amounts, seen, supplyAtRead)`; the desk already sends it, and the Robinhood
keeper drops reports whose supply moved), with `minted = supplyAtRead + seen`. Two `uint256` mappings,
`lowSupply[id]` and `burnedAtReport[id]` (or one packed slot); `_burned` sets
`lowSupply[id] = min(lowSupply[id], totalSupply)` after its burn; `_report` as above. No new events or errors
(`BadParams` for the new array's length). The Foundry regressions to go with it: back 80 of 100 at 60c, take a
read, mint 10 and redeem 10, report the read: backing 80 and risk 20 x 0.4 (v14 gives backing 70; a later report of
80 then leaves 10 x 0.4). Back 100 of 100, read, redeem 10, mint 10, report 100: backing 90 and risk 10 x 0.4.
Fifty round trips of 100 at 50c between a read of 900 of 1,000 and its report: backing 900, risk 100 x 0.5.
The stale read reported twice above: backing 67 both times. On the EVM `seen` does enter the bound, through
`minted = supplyAtRead + seen`; a `seen` below the truth only lowers it, and one above is skipped.

**sendToBridge(amount)**, onlyKeeper, nonReentrant: `bridgeDeposit == 0` -> `NoBridge`; `amount > freeFloat()` ->
`InsufficientFloat(amount, free)`; transfer; emit `SentToBridge`. It is not blocked by `halted`. Delayed sales are
not reserved, so the bridge may take that USDG and the release then queues.

**setRoles(keeper_, factory_, bridgeDeposit_)**, onlyOwner: if `bridgeDeposit_ != bridgeDeposit`: if it is non-zero,
`bridgeEverSet` -> `BridgeChangeDelayed()`, else set `bridgeEverSet = true`; then assign (clearing to zero is always
instant). Sets keeper and factory. Emits `RolesSet(keeper, factory, bridgeDeposit)`.

**proposeBridgeDeposit(addr)**, onlyOwner: pending = addr; at = now + BRIDGE_DELAY; emit. There is no expiry.

**acceptBridgeDeposit()**, onlyOwner: `pendingAt == 0` -> `NothingPending`; `now < pendingAt` -> `NotYet`; assign; if
non-zero, `bridgeEverSet = true`; clear pending; emit `RolesSet`.

**setParams(buy, sell, minP, maxP, defaultMaxRisk)**, onlyOwner: `buy > 500 || sell > 500 || minP < 1_000 || minP >=
maxP || maxP >= ONE` -> `BadParams`; set; emit `ParamsSet`.

**setMaxRisk(id, cap)**, onlyOwner. No event. 0 means use the default.

**setShareLabel(id, name, symbol)**, keeper or owner (`OnlyKeeper`): no pToken, `name > 64B` or `symbol > 16B` ->
`BadParams`; `p.setLabel`; emit.

**setMaxTotalRisk(cap)**, onlyOwner; emit.
**setSettleFee(bps)**, onlyOwner; `> 200` -> `BadParams`; emit.
**setMeterSales(bool)**, onlyOwner; emit.
**setOutflowCap(floor, floatBps)**, onlyOwner; `floatBps > BPS` -> `BadParams`; emit.
**halt()**, keeper or owner (`OnlyKeeper`); emit `HaltSet(true)`.
**resume()**, onlyOwner; emit `HaltSet(false)`.
**rescue(token, to, amount)**, onlyOwner: `token == usdg || positionIdOf[token] != 0` -> `NotRescuable`; transfer;
emit `Rescued`.
**refreshRisk(ids[])**, anyone: for each id with risk != 0: settled -> store 0; else store
`_bounded(risk, supply, backed, low)`. It only lowers risk.
**freeFloat, maxRisk, mintOut, redeemOut, outflowRemaining**: views.
**renounceOwnership** -> `BadParams`.

**_markHour()**: `hour = now/3600`; if `hourFloat.hour == hour`, return; `cur = freeFloat()`; `hourFloat = (hour,
u96(cur), u96(_base(old, cur)))`. It is called by `_mint`, `_mintSettled` and `_redeem` only (not by release,
payQueue or sendToBridge).

### Events
PTokenCreated, Minted, Redeemed, Absorbed, BackedReported, SentToBridge, ParamsSet, RolesSet, BridgeDepositProposed,
Rescued, OutflowCapSet, MeterSalesSet, SaleDelayed, Released, Cancelled, HaltSet, Queued, ClaimPaid, ClaimUndelivered,
UnclaimedWithdrawn, SettleFeeSet, MaxTotalRiskSet, ShareLabelSet.

### Invariants (enforced or relied on; the harness checks them)
1. **USDG ledger.** `queued == sum(live claims[claimHead..]) + unclaimedTotal`. `freeFloat` excludes `queued`.
   `_payQueue` pays from `balance - unclaimedTotal`. Set-aside USDG is never paid to a later claim.
2. **Strict FIFO queue.** The head blocks everything behind it until the float covers it. A recipient whose transfer
   fails is moved to `unclaimed` (it is not lost and does not block). A global USDG failure halts the loop without
   reordering.
3. **Quote fill.** `quoteFilled[digest] <= q.maxAmount` across all transactions. A quote binds no taker.
4. **backed <= totalSupply** at report time (clamped). Every burn lowers `backed` by the part not attributed to
   `mintedSinceReport`. `seen` is never ahead of `burnedTotal`.
5. **Risk.** `totalRisk == sum(uint128(unbackedRisk[id]))`. Per-market `risk/ONE <= maxRisk` and `totalRisk/ONE <=
   maxTotalRisk` after every quoted mint. A settled market carries 0 (at the next burn, report, refresh or final
   settled mint).
6. **Delayed shares.** `delayedShares[id] ==` the pToken held by the exchange for live tickets. `absorb` burns only
   the excess.
7. **Outflow.** Metered USDG paid or queued in any sliding 3600s window is at most the snapshot cap (about two caps
   across a boundary). The cap base is the free float at the first trade of the hour, min with the previous
   snapshot. Gross, not net of mints.
8. **Settlement.** A settled market redeems at the payout less `settleFeeBps` (always metered). Mints at the payout
   plus the buy spread need no quote. A pending $1 settlement mints at $1 at once. A $0 settled market cannot mint
   (`SettledAtZero`) and redeems for 0 (burn only).
9. `halted` stops mint, redeem, release and payQueue. It does not stop withdrawUnclaimed, cancel, sendToBridge,
   reportBacked or absorb.
10. The bridge destination changes only after 2 days, except the very first set. Clearing is instant.

### Solana redesign
- **Program layout.** Put the oracle and exchange in one program (`polypad_exchange`) to avoid CPI view calls.
  Accounts:
  - `Config` PDA `["exchange"]`: owner, pending_owner, keeper, factory (or the factory program id), bridge_deposit,
    pending_bridge, pending_bridge_at, bridge_ever_set, spreads, min/max price, meter_sales, default_max_risk,
    max_total_risk, total_risk (u128), outflow_floor, outflow_float_bps, halted, settle_fee_bps, queued, claim_head,
    claim_tail, unclaimed_total, delayed_next, hour_float {hour, amount, base}, outflow {cur_hour, cur_amount,
    prev_hour, prev_amount}.
  - **USDG vault**: a token account owned by the `["exchange"]` PDA (`["float"]`). Use `vault.amount` as the balance.
    Direct donations still count, as in the EVM version.
  - `Market` PDA `["market", position_id]`: oracle status fields + backed, max_risk_override, risk (u128), low, high,
    minted_since_report, minted_total, burned_total, low_supply, burned_at_report, delayed_shares, pToken mint
    pubkey, label.
  - `QuoteFill` PDA `["fill", digest]` (u64 filled, valid_until). Created with `init_if_needed`, rent paid by the
    taker. Add a permissionless `close_fill` after `valid_until + MAX_VALIDITY` that refunds rent to the payer. A closed
    fill can never be replayed because the quote is expired. Alternative: a bounded ring of recent digests in one
    zero-copy account. A PDA is simpler.
  - `Claim` PDA `["claim", index u64]` {to, amount u64, payer}: the queue is `claim_head..claim_tail`. `pay_queue`
    takes the claim PDAs for `head..head+max` in `remaining_accounts` and closes them. Rent goes back to the payer.
    The **recipient token account must be passed too**; for that, pre-read the owner and derive the ATA.
  - `Unclaimed` PDA `["unclaimed", owner]`.
  - `DelayedSale` PDA `["delayed", ticket u64]` {to, ready_at, position_id, p_amount, amount}. Close on
    release or cancel.
  - Outflow per hour: the mapping is only ever read for `hour` and `hour-1`, so store two slots (cur, prev) and roll
    them when the hour changes.
- **_tryTransfer / try-catch do not exist on Solana**: a failed CPI aborts the whole transaction. Redesign it this
  way. Before transferring, inspect the destination token account. If it is missing, not initialized, has the wrong
  mint, or is `AccountState::Frozen`, treat it as "failed for this recipient" and set it aside. Treat "USDG globally
  paused" (the Token-2022 Pausable extension, if USDG on Solana has it) as a global failure: stop the queue, or revert
  for release. Check the actual Solana USDG mint's extensions (Paxos USDG on Solana is Token-2022; check for
  PermanentDelegate, Pausable, TransferHook, DefaultAccountState). **Pay to a token account, not a wallet.** Store the
  owner wallet in the claim and require the ATA. If the ATA does not exist, set the claim aside rather than create it
  (the queue payer should not fund ATAs for others).
- `safeTransferFrom(msg.sender)` -> a `transfer_checked` CPI with the user as signer authority. There is no
  approve step.
- `nonReentrant` -> not needed. Solana forbids reentry into a program through CPI except direct self-recursion. Keep
  CPI ordering checks-effects-interactions anyway.
- `msg.sender` roles -> `Signer` accounts constrained with `has_one = keeper` / `owner`. Ownable2Step becomes
  `pending_owner` and an `accept_owner` instruction.
- uint256 math -> u128 is enough for every value: shares are at most about 1e15 and risk is at most about 1e21 x 1e6
  = 1e27. Use checked math. SafeCast u96 -> u64 (USDG amounts fit).
- `halted` and the hourly snapshot read the float balance: pass the vault account into every instruction that calls
  `_markHour`.
- `Minted`/`Redeemed`/etc. -> `emit_cpi!`, which is more reliable for indexers than `msg!` logs that get truncated.
- Absorb on Solana: "pToken held by the exchange" is the market's `held` PDA minus `delayed_shares`, plus any
  other pToken account the exchange PDA owns (its ATA for that mint, passed as `other`). Fee shares may go to
  either.
- **Burns outside the exchange.** PToken.burn is exchange-only on the EVM; an SPL Token holder can burn their own
  balance. Supply then drops with no exchange call, so invariant 4 can break between instructions. The program
  books those burns whenever it next reads the supply (`Market.minted_total - supply - burned_total`), as `_burned`
  books the same shares redeemed: backing is clamped to the supply, `minted_since_report` absorbs them first and
  `(ONE - high)` each comes off the risk for those, the rest lower backing, and no instruction ever fails over them.
  The `_bounded` at the lower supply that the same instruction applies next frees the room the burned shares used,
  as an exchange burn would, and no more. The supply it reads also lowers `low_supply` (the report's bound). After
  any instruction on a market, `backed <= supply` and `burned_total + supply == minted_total` hold again.
- **`_report` with the read's `minted`** (deliberate divergence from v14): `report_backed(amounts, seen, minted)`.
  The report credits no more than the lowest supply since the read and, with nothing lost, never lowers backing;
  `Market` keeps `low_supply` and `burned_at_report` for it. See the note under `_report` above, which also gives
  the equivalent EVM change.

---

## 4. LaunchFactory

**Purpose.** Permissionless, free coin launch on a Polymarket outcome. It needs a live BUY quote inside the
exchange's band, which sizes the curve (the phantom reserve). It creates the market's pToken on first use, deploys the
BondingCurve and Coin, and registers them with the FeeVault.

### Constants
| name | value |
|---|---|
| MAX_CURVE_FEE_BPS | 200 (2%) |
| MAX_POOL_FEE | 15_000 (1.5%, v4 hundredths of a bip) |
| MIN_CREATOR_SHARE_BPS | 3_000 |
| MAX_CREATOR_SHARE_BPS | 8_000 |

### State
| var | type | default | writer |
|---|---|---|---|
| exchange | PExchange immutable | ctor | |
| oracle | PriceOracle immutable | ctor | |
| platform | address | ctor | setConfig |
| gradUsd | uint256 USDG 6dp | 6_000e6 | setConfig |
| graduator | Graduator | 0 | setGraduator |
| fees | Fees{curveFeeBps u16, curveCreatorShareBps u16, poolFee u24, poolCreatorShareBps u16} | (140, 5_000, 10_000, 5_000) | setFees |
| feeVault | FeeVault | 0 | setFeeVault |
| curves | address[] | | launch |
| isCurve | mapping(address => bool) | | launch |

### Functions
**setConfig(platform_, gradUsd_)**, onlyOwner: `platform_ == 0 || gradUsd_ < 1_000e6 || gradUsd_ > 1_000_000e6` ->
`BadParams`. Emits `ConfigSet`.
**setGraduator(g)**, onlyOwner, unchecked; emit. **setFeeVault(v)**, onlyOwner, unchecked; emit.
**setFees(f)**, onlyOwner: `curveFeeBps == 0 || > 200 || poolFee == 0 || > 15_000 || curveCreatorShareBps outside
[3000, 8000] || poolCreatorShareBps outside [3000, 8000]` -> `BadFees`. Emits `FeesSet`.

**launch(positionId, name, symbol, metadataURI, holdersBps, q, sig)**, anyone:
1. `q.positionId != positionId` -> `QuoteForOtherMarket`
2. `graduator == 0 || feeVault == 0` -> `NoGraduator`
3. `holdersBps > 10_000` -> `BadHoldersShare`
4. `name > 64B || symbol > 16B || metadataURI > 512B` -> `BadParams`
5. `_launchPrice`: `price = oracle.verify(q, sig, BUY)` (all oracle checks; market must be unpaused and not settled);
   `price < exchange.minPrice() || > exchange.maxPrice()` -> `PriceOutOfBand`. **The quote is not consumed**
   (`quoteFilled` is untouched; `verify` is a view). It can be reused for many launches within its life.
6. `_deploy`:
   - `p = exchange.ensurePToken(positionId)` (the factory must be set as `exchange.factory`)
   - `phantom = floor(gradUsd * 2e6 / (price * 5))` (pToken 6dp). This is 0.4 x target, where `target =
     gradUsd/price` shares.
   - `curve = new BondingCurve(p, exchange, creator = msg.sender, platform, phantom, graduator, feeVault, fees)`
   - `coin = new Coin(name, symbol, metadataURI, curve, holdersBps > 0 ? feeVault : 0)`, which mints 1e27 to the curve
   - `feeVault.register(coin, curve, graduator, p, msg.sender, holdersBps)`
   - `curve.initialize(coin)`
   - `curves.push`; `isCurve[curve] = true`
   - emit `Launched(positionId, creator, coin, curve, pToken, price, phantom, holdersBps)`

**curveCount() view**. **renounceOwnership** -> `BadParams`.

### Solana redesign
- One `polypad_launch` program (or merge it with the curve). `launch` creates: the coin mint (PDA `["coin", curve]`
  or a client keypair), the curve PDA `["curve", coin_mint]`, the curve's coin vault and pToken vault (ATAs owned by
  the curve PDA), and the FeeVault `Launch` record PDA `["launch", coin_mint]`. It mints the full supply to the coin
  vault and then **revokes mint authority** (fixed supply). It creates metadata (Metaplex or Token-2022 metadata
  extension). The account count is large: split it into `launch_init` + `launch_finish`, or use an ALT.
- `isCurve` -> derive and check the PDA `["curve", coin_mint]` owned by the program. The Graduator's `OnlyCurve` check
  becomes "signer is the curve PDA via invoke_signed" or a direct internal call if it is the same program.
- `ensurePToken` -> CPI into the exchange program `ensure_ptoken`, which `init_if_needed` the pToken mint PDA and the
  Market PDA.
- String limits are the same. The rent cost of the metadata is paid by the creator.
- `curves[]` enumeration -> drop it; indexers use `getProgramAccounts` or events.

---

## 5. BondingCurve

**Purpose.** A per-coin constant-product curve that trades the coin against the market's pToken over a phantom
(virtual) pToken reserve. 2/7 of supply is reserved for the pool. Sellout graduates the curve atomically into a
Uniswap v4 pool, and a failure there is retryable.

### Constants and immutables
| name | value |
|---|---|
| BPS | 10_000 |
| DEAD | 0x...dEaD |
| GRADUATION_GAS | 1_200_000 |
| pToken, exchange, creator, platform, factory (= deployer), graduator, feeVault | immutable |
| feeBps | fees.curveFeeBps (default 140) |
| creatorShareBps | fees.curveCreatorShareBps (default 5_000) |
| poolFee | fees.poolFee (default 10_000 = 1%) |
| poolCreatorShareBps | default 5_000 |
| phantom | `gradUsd*2e6/(price*5)` pToken 6dp |

### State
| var | type | default | writer |
|---|---|---|---|
| coin | IERC20 | 0 | initialize |
| reserved | uint256 coin 18dp | `floor(balance*2/7)` = 285_714_285_714285714285714285 (2/7 x 1e27 floored) | initialize |
| trackedQuote | uint256 pToken 6dp | 0 | buy (+net), sell (-gross), graduate (=0) |
| trackedTokens | uint256 coin 18dp | 1e27 | buy (-out), sell (+coinsIn), graduate (=0) |
| graduated | bool | false | _graduate |

### Formula (virtual reserves)
```
x = phantom + trackedQuote      (virtual pToken reserve)
y = trackedTokens               (real coins held, reserve included)
k = x*y, starting at phantom * SUPPLY
sellable = trackedTokens - reserved
target raise q* = 2.5 * phantom = gradUsd / launchPrice  (shares)
   since phantom*SUPPLY = (phantom+q*)*reserved, reserved = 2/7 SUPPLY  =>  q* = 2.5 phantom
spot price (pToken per whole coin, 6dp) = (phantom + trackedQuote) * 1e18 / trackedTokens
initial spot = phantom*1e18/1e27 ; final spot = 3.5*phantom*1e18/(2/7 * 1e27) = 12.25x initial
```
Example: gradUsd $6,000 at a 50c launch price -> target 12,000 shares, phantom 4,800 shares (4_800e6).

### Functions
**initialize(coin_)**: factory only (`OnlyFactory`); already set -> `AlreadyInitialized`; `trackedTokens =
coin.balanceOf(this)`; `reserved = trackedTokens*2/7`.

**buy(quoteIn, minOut, to)**, nonReentrant, anyone:
1. `coin == 0` -> `NotInitialized`; `quoteIn == 0` -> `ZeroAmount`; `graduated` -> `Graduated_`
2. `sellable = trackedTokens - reserved`; `sellable == 0` -> `SoldOut_`
3. `fee = ceil(quoteIn * feeBps / BPS)` (`_feeOn` rounds up); `net = quoteIn - fee`; `v = phantom + trackedQuote`;
   `out = floor(net * trackedTokens / (v + net))`
4. If `out >= sellable` (this buy sells out the curve):
   - `out = sellable`
   - `net = ceil(v * out / (trackedTokens - out))`, computed as `(v*out + (T-out) - 1)/(T-out)`; `net = min(net, quoteIn)`
   - `fee = ceil(net * feeBps / (BPS - feeBps))` (the fee as a share of the gross); if `net + fee > quoteIn`, `fee =
     quoteIn - net`
   - `refund = quoteIn - net - fee`; emit `SoldOut()`
5. `out == 0` -> `ZeroAmount`; `out < minOut` -> `Slippage`
6. `trackedQuote += net`; `trackedTokens -= out`
7. `pToken.safeTransferFrom(msg.sender, this, quoteIn - refund)`. **The refund is never pulled**, so the caller keeps
   it.
8. `coin.safeTransfer(to, out)`; `_payFee(fee)`; emit `Buy(msg.sender, to, quoteIn - refund, out, fee, refund)`
9. If `trackedTokens == reserved`, `_graduate()`.

**sell(coinsIn, minOut, to)**, nonReentrant:
1. `NotInitialized`, `ZeroAmount`, `Graduated_` checks
2. `gross = floor(coinsIn * v / (trackedTokens + coinsIn))`; `gross = min(gross, trackedQuote)`
3. `fee = ceil(gross * feeBps / BPS)`; `out = gross - fee`; `out == 0` -> `ZeroAmount`; `< minOut` -> `Slippage`
4. `trackedQuote -= gross`; `trackedTokens += coinsIn`
5. Pull coins from `msg.sender`; send `out` pToken to `to`; `_payFee(fee)`; emit `Sell`.
- Sells work while the curve is sold out but not yet graduated (the graduation-failed state), which un-sells it out.

**graduate()**, nonReentrant, anyone: `graduated` -> `Graduated_`; `trackedTokens != reserved` -> `NotSoldOut`;
`_graduate()`.

**_graduate()**:
1. `gasleft() < GRADUATION_GAS` -> `NeedsGasToGraduate`. This reverts the whole buy.
2. `q = trackedQuote`; `poolCoins = floor(reserved * q / (phantom + q))`. The pool opens at the curve's final price
   `(phantom+q)/reserved`. Nominally `poolCoins = 5/7 reserved = 10/49 SUPPLY ≈ 20.41%`, and `burned = reserved -
   poolCoins = 4/49 SUPPLY ≈ 8.16%`.
3. Approve the graduator for `poolCoins` coin and `q` pToken.
4. `try graduator.graduate(coin, pToken, poolCoins, q, feeVault, platform, poolFee, poolCreatorShareBps)`:
   - success: `graduated = true`; `burned = trackedTokens - poolCoins`; `trackedQuote = trackedTokens = 0`; send
     `burned` coins to DEAD (a transfer, not a supply burn); emit `Graduated(poolId, poolCoins, q, burned)`.
   - catch: reset the approvals to 0; emit `GraduationFailed(reason)`. The buy still succeeds.

**_payFee(fee)**: `toCreator = floor(fee * creatorShareBps / BPS)`; if > 0, approve and `feeVault.deposit(coin,
pToken, toCreator)`; the rest (`fee - toCreator`) goes to `platform` via pToken transfer.

**Views**: `quoteBuy` (clamped to sellable; no refund math), `quoteSell`, `spotPrice` (the graduator's pool price
after graduation), `target() = phantom*5/2`, `soldOut()`.

### Invariants
- The curve's pToken balance is at least `trackedQuote` (fees are paid out immediately; donations are ignored because
  reserves are tracked).
- `trackedTokens >= reserved` while not graduated. The curve never sells the reserve.
- Rounding favours the curve: buy output and sell output are floored, fees are ceiled, and the sell-out net is ceiled.
- Fees are fixed per coin (immutables).
- Graduation is atomic with the sell-out buy when it can be. Otherwise the curve stays sold out with sells open, and
  anyone can retry.

### Solana redesign
- **try/catch around graduation is impossible**: CPI failure aborts. Options: (a) graduation is a separate
  permissionless `graduate` instruction (crank). The sellout buy only marks `sold_out`, and the Router or client
  appends a `graduate` instruction in the same transaction when it can. That instruction is atomic, so if it fails the
  client resends without it. (b) Graduate inline and let a failure revert the buy. (a) matches the EVM semantics best.
  `GRADUATION_GAS` -> a compute-unit check is not possible from inside the program. Request CUs client-side. With (a)
  it is moot.
- Coins are 18 dp with 1e27 supply. **SPL amounts are u64.** Use 6 decimals (supply 1e15, as on pump.fun) or 9
  decimals (1e18, which fits u64 < 1.8e19). Rescale all formulas: `reserved = SUPPLY*2/7`, spot price uses
  `10^decimals`. Intermediate products: `net(≤~1e15) * trackedTokens(≤1e18)` = 1e33, which fits u128 (3.4e38).
- The coin vault and pToken vault are ATAs owned by the curve PDA. Use vault balances only for transfers and keep
  `tracked_*` fields for the math (keep the donation-proof property).
- "Send to DEAD" -> an SPL `burn` (a real supply reduction) from the curve vault. The FeeVault holder-coin "burn" is
  the same.
- `forceApprove` + pull -> direct CPI transfers signed by the curve PDA. There are no approvals.
- `isCurve` -> PDA derivation.

---

## 6. Coin

**Purpose.** A fixed-supply (1e27, 18dp) ERC-20 minted entirely to its curve. If the coin has a holder share
(`holdersBps > 0`), every transfer calls `FeeVault.onTransfer(from, to)`, so the vault can track balances for reward
accrual.

### State
`SUPPLY = 1_000_000_000e18` constant; `metadataURI` string (set in the ctor); `holderBook` immutable (FeeVault or 0).

### Functions
- ctor: ERC20(name, symbol); `_mint(curve, SUPPLY)`. That calls `_update` and therefore `onTransfer(0, curve)` if
  `holderBook` is set. At that point the FeeVault `launches[coin]` is not registered yet (register runs after), so the
  call returns early because `holdersBps == 0`.
- `_update` override: `super._update`, then if `holderBook != 0`, `IHolderBook.onTransfer(from, to)`. This is
  deliberately **not** in try/catch: a hook that fails reverts the transfer, so a gas-starved hook can never skip
  bookkeeping.

### Solana redesign (one of the hardest items)
- Option A: a **Token-2022 mint with the TransferHook extension** pointing at the FeeVault program. The hook's
  `execute` receives source and destination token accounts and the amount, plus ExtraAccountMetas (the holder PDAs
  `["holder", coin, owner]` and the Book PDA). Constraints: the hook gets **de-escalated signer privileges, so it cannot
  pay rent** to create holder PDAs. Holder PDAs must be created beforehand (a `register_holder` instruction, or created
  lazily by the FeeVault on first claim, with untracked holders accruing nothing until then; that changes semantics).
  Also, many AMMs, aggregators and CEX deposit flows handle Token-2022 transfer hooks poorly. Raydium CPMM supports
  hooks only for whitelisted programs, and Meteora DAMM v2 supports some extensions. Check this before relying on it.
- Option B (simpler, more compatible): a classic SPL coin and **no on-chain per-transfer tracking**. Holder rewards
  become an off-chain time-weighted computation (indexer) published as periodic merkle roots, which holders claim
  from the FeeVault, or which the keeper pushes. This changes trust: the keeper computes the rewards. It needs a user
  decision.
- Option C: hybrid. A hook that only updates holder PDAs if they exist and passes otherwise, plus an explicit
  `track_me`. Holders who never register forfeit.
- Coins with `holdersBps == 0` should be plain SPL or Token-2022 without the hook (cheaper, more compatible).
- `metadataURI`: Metaplex Token Metadata `uri`, or the Token-2022 metadata extension.

---

## 7. Fees (struct)

`Fees { uint16 curveFeeBps; uint16 curveCreatorShareBps; uint24 poolFee; uint16 poolCreatorShareBps }`. `poolFee` is
in v4 units (hundredths of a bip: 10_000 = 1%). The creator share is in bps of the fee, and the remainder goes to the
platform. Defaults: curve 1.4%, creator side 50% (0.7% creator side / 0.7% platform); pool 1%, 50/50.

Solana: copy it into the Curve account at launch. Express the pool fee in the target AMM's units (Raydium CPMM
`trade_fee_rate` in 1e-6 units, which is the same scale as v4's hundredths of a bip; Meteora uses bps or its own
scale).

---

## 8. FeeVault

**Purpose.** Receives every coin's creator-side fees, from the curve (in pToken) and the pool (in pToken and coin).
It credits the coin's payee with `(1 - holdersBps)` and streams the holders' part in pToken over one hour to holders
via a masterchef accumulator; the holders' part in coin is burned. Payout is pull or push (`claimFor`), with an
optional cash-out to USDG through the exchange.

### Constants
| name | value |
|---|---|
| BPS | 10_000 |
| STREAM | 1 hour |
| ACC | 1e36 |
| MAX_ACC | type(uint256).max / 1e27 |
| MIN_SUPPLY | 100_000e18 (0.01% of supply) |
| DEAD | 0x...dEaD |

### State
| var | type | default | writer |
|---|---|---|---|
| factory | immutable | | |
| exchange | PExchange immutable | | |
| poolManager | address immutable (excluded from rewards) | | |
| launches[coin] | Launch{curve, graduator, pToken, payee, holdersBps u16} | | register, setPayee |
| books[coin] | Book{acc uint256 (pToken per coin x ACC; monotonic), supply uint256 (sum of tracked balances), pot uint256 (released, unpaid), streaming uint256 (deposited, unreleased), last u64, end u64} | 0 | deposit, _release, _settle, _debit, _payOne |
| tracked[coin][holder] | uint256 coin balance | 0 | _settle |
| debt[coin][holder] | uint256 (= bal x acc) | 0 | _settle |
| rewards[coin][holder] | uint256 pToken | 0 | _settle, _debit, _payOne |
| owed[asset][payee] | uint256 | 0 | deposit, withdraw, withdrawUsd |

### Functions
**register(coin, curve, graduator, pToken, creator, holdersBps)**: factory only (`OnlyFactory`); `holdersBps > BPS ||
already registered` -> `BadParams`; store; emit `Registered`.

**setPayee(coin, payee)**: `msg.sender != l.payee` -> `OnlyPayee`; `payee == 0` -> `BadParams`; set; emit. Old owed
balances stay with the old payee (`owed` is keyed by asset and payee, not by coin).

**deposit(coin, asset, amount)**, anyone (donations allowed):
1. `l.curve == 0` -> `Unknown(coin)`; `asset != l.pToken && asset != coin` -> `BadAsset`; `amount == 0` -> return
2. Pull `amount` from the caller
3. `toHolders = floor(amount * holdersBps / BPS)`; `toPayee = amount - toHolders`; `owed[asset][payee] += toPayee`
4. If `toHolders > 0`:
   - asset == coin: transfer to DEAD (this triggers `onTransfer`; DEAD is excluded)
   - else: `_release(coin)`; then a weighted stream end: `left = max(0, end - now)`; `total = streaming + toHolders`;
     `end = now + floor((streaming*left + toHolders*STREAM)/total)`; `streaming = total`
5. emit `Deposited(coin, asset, toPayee, toHolders)`

**withdraw(asset, to)**, nonReentrant: `amount = owed[asset][sender]`; 0 -> `NothingToClaim`; zero it; transfer; emit
`Withdrawn(asset, sender, to, amount, 0)`.

**withdrawUsd(pToken, to, minOut, q, sig)**, nonReentrant: `amount = owed[pToken][sender]`; 0 -> `NothingToClaim`;
zero it; `out = exchange.redeem(pToken, amount, minOut, to, q, sig)` (the vault is the burner, holding its own
pToken); emit. A revert in redeem reverts it all. Note that an `owed` balance in a *coin* asset cannot go through
`withdrawUsd`, because `_market` would revert.

**onTransfer(from, to)**: not nonReentrant. If `launches[msg.sender].holdersBps == 0`, return (the caller is not a
registered holder coin). `_release(coin)`; `_settle(coin, from)`; `_settle(coin, to)`.

**claim(coin, to)**, nonReentrant: `pToken == 0` -> `Unknown`; `_release`; `_settle(sender)`; `amount =
_debit(coin, sender)`; 0 -> `NothingToClaim`; transfer pToken to `to`; emit `HolderPaid(..., false)`.

**claimFor(coin, holders[], minAmount, cashOut, q, sig)**, nonReentrant, anyone: `Unknown` check; `_release`; for each
holder `_payOne`:
- `_settle(h)`; `amount = min(rewards[h], pot)`; if `0 || < minAmount`, skip (return 0)
- `_debit(h)`
- `cashOut = cashOut && h == msg.sender` (only the holder may force a sale)
- success = cashOut ? `try exchange.redeem(p, amount, 0, h, q, sig)` : `_tryTransfer(p, h, amount)`
- on success: emit `HolderPaid`, return amount. On failure: `rewards[h] += amount; pot += amount;` emit
  `HolderSkipped`.
- Note: in cash-out mode, one quote is used across multiple redeems (only the self entry can cash out, so effectively
  once). The quote's `maxAmount` must cover it.

**pending(coin, user) view**: 0 if excluded; else `min(rewards + (tracked*acc' - debt)/ACC, pot + released')` using
`_preview`.

**Internal**:
- `_excluded(coin, u)`: `u ∈ {0, DEAD, this, poolManager, l.curve, l.graduator}`.
- `_settle(coin, u)`: if excluded, return; `earned = floor((tracked*acc - debt)/ACC)`; `rewards += earned`; `bal =
  coin.balanceOf(u)`; if `bal != tracked`, `supply = supply - tracked + bal; tracked = bal`; `debt = bal*acc`.
- `_release(coin)`: if `last == now`, return; `amount = _releasable`; `last = now`; if `amount == 0`, return; if `supply
  < MIN_SUPPLY`: `end = now + STREAM`; return (the money is held and restreamed). `step = floor(amount*ACC/supply)`;
  `step = min(step, MAX_ACC - acc)`; if `step == 0`, return; `acc += step`; `applied = ceil(step*supply/ACC)`;
  `streaming -= applied`; `pot += applied`; emit `Released(coin, applied, supply)`.
- `_releasable(b)`: `streaming == 0` -> 0; `now >= end || last == 0` -> streaming; else
  `floor(streaming*(now - last)/(end - last))`.
- `_debit`: `amount = min(rewards, pot)`; subtract from both.

### Invariants
- Solvency: the vault's pToken balance per pToken is at least `sum over coins (pot + streaming) + sum over payees
  owed[pToken]`. The pot takes the ceiling of what was promised, so holders can always be paid in full.
- `books.supply == sum of tracked balances of non-excluded holders`, which equals their real balances after each
  transfer (thanks to the synchronous hook).
- `acc` is monotonic and capped at `MAX_ACC`.
- Excluded addresses never accrue.
- Below `MIN_SUPPLY`, nothing is released.
- `holdersBps` is fixed per coin. `payee` is transferable only by the payee.

### Solana redesign
- PDAs: `Launch` `["launch", coin]` (the Book can sit in the same account), `Holder` `["holder", coin, owner]`
  {tracked, debt (u128 or U256), rewards}, `Owed` `["owed", asset_mint, payee]`. Token vaults: per-pToken-mint ATAs owned
  by `["fee_vault"]`.
- **The accumulator needs U256**: `tracked (≤1e27, or ≤1e15/1e18 after rescale) * acc (≤MAX_ACC)` overflows u128. With
  6-decimal coins (supply 1e15), rescale `ACC` to 1e24 or so and keep `MAX_ACC = u128::MAX / supply`. Alternatively
  use the `uint` / `primitive-types` U256 crate (more compute) or a 1e18-scaled u128 accumulator with care. Recompute
  the overflow bounds.
- "Owner" vs "token account": Solana balances are per token account and an owner can hold several. Track per **owner
  wallet**, summing ATAs, or per **token account** (simplest in a hook, since the hook sees token accounts). Recommend
  per token account, with rewards claimable by its owner.
- `balanceOf(user)` inside `_settle` becomes the token account's `amount` passed in.
- Exclusions become the curve vault, the pool vaults, the fee-vault ATA, the graduator/locker, and the burn (no DEAD:
  use a real burn). Pool vault addresses depend on the AMM and must be recorded at graduation.
- `_tryTransfer` / `try exchange.redeem` -> impossible to catch. Pre-check the destination token account (exists, not
  frozen, right mint) and skip it otherwise. For cash-out, run the self-claim as its own instruction (no batch) and
  let it revert.
- `withdrawUsd` -> CPI into the exchange `redeem` with the vault PDA as the burn authority (`invoke_signed`).
- Push payouts (`claimFor`): each holder's ATA goes in `remaining_accounts`. Expect about 10-20 holders per transaction.

---

## 9. Graduator

**Purpose.** At sellout it creates one Uniswap v4 pool (coin/pToken, the coin's fixed LP fee, tick spacing 200, a hook
that is this contract) with one full-range position that is never removed (liquidity locked forever). It lets anyone
`collect` accrued pool fees, split creator side -> FeeVault and the rest -> platform, in both tokens.

### Constants and immutables
`TICK_SPACING = 200`; `BPS = 10_000`; `poolManager` immutable; `factory` immutable. Full range: `upper =
(MAX_TICK/200)*200 = 887_200`, `lower = -887_200`. The hook address is CREATE2-salt-mined so that its low 14 bits equal
`BEFORE_INITIALIZE_FLAG` (0x2000).

### State
`pools[coin] = Pool{PoolKey key; address feeVault; address platform; uint128 liquidity; uint16 creatorShareBps}`. A
coin is graduated iff `liquidity != 0`.

### Functions
**graduate(coin, pToken, coins, pTokens, feeVault, platform, fee, creatorShareBps)**, nonReentrant:
1. `!IsCurve(factory).isCurve(msg.sender)` -> `OnlyCurve`
2. `pools[coin].liquidity != 0` -> `AlreadyGraduated`
3. `before = [coin.bal(this), pToken.bal(this)]`; pull both from the curve
4. `key = {currency0 = min(coin, pToken), currency1 = max, fee, 200, hooks = this}`
5. `_seed(key, amount0, amount1)`:
   - `sqrtPriceX96 = floor(sqrt(amount1 * 2^192 / amount0))`
   - `poolManager.initialize(key, sqrtPrice)` (which calls back `beforeInitialize`, requiring `sender == this`)
   - `liquidity = _liquidity(sqrtP, sqrtA(lower), sqrtB(upper), a0, a1)`:
     `l0 = a0 * (sqrtP*sqrtB/Q96) / (sqrtB - sqrtP)`; `l1 = a1 * Q96 / (sqrtP - sqrtA)`; `l = min(l0, l1)`;
     return `l - l/1e9 - 1` (a hair under, so the settle never falls short)
   - `poolManager.unlock(abi.encode(key, liquidity))` -> `unlockCallback` -> `modifyLiquidity(full range, +liquidity,
     salt 0)` -> settle the negative deltas by transfer (sync/transfer/settle) and take any positive ones
6. Store the Pool
7. `_sweepDust`: the balance delta vs `before` for both tokens goes to the platform (only this graduation's dust)
8. emit `Graduated(coin, pToken, poolId, coins - dust0, pTokens - dust1, liquidity)`; return poolId

**collect(coin)**, nonReentrant, anyone: `liquidity == 0` -> `NotGraduated`; `unlock(key, 0)` (a 0-liquidity
modifyLiquidity realizes fees, and the positive deltas are taken to this contract); `fee0 = t0.balanceOf(this)`, `fee1
= t1.balanceOf(this)` (the **entire** balance, not a delta); `_split` each; emit `FeesCollected`.
- `_split(coin, t, amount, p)`: `toCreator = floor(amount * creatorShareBps / BPS)` -> approve and
  `FeeVault.deposit(coin, t, toCreator)` (for the coin asset, the holder part is burned; for pToken, it streams); the
  rest goes to the platform.

**unlockCallback(data)**: only the poolManager (`OnlyPoolManager`).
**beforeInitialize(sender, key, sqrtP)**: only the poolManager; `sender != this` -> `OnlySelf`. This stops anyone else
from initializing a pool with this hook at a bad price before graduation.
**Views**: `poolKey` (reverts `NotGraduated`), `poolOf`, `spotPrice` (pToken per whole coin 6dp from sqrtP; the
orientation depends on currency order), `graduated`.

### Invariants
- One pool per coin. The liquidity is never removed (no remove function exists), so the LP is locked forever.
- The pool opens at the curve's last price, `q/poolCoins ≈ (phantom+q)/reserved`.
- Dust goes only to the platform, measured as a delta, so donations cannot block graduation.
- Third-party liquidity in the same pool shares fees (accepted).

### Solana redesign
- There is no Uniswap v4. Choices:
  1. **Our own CPMM pool inside the program** (recommended for control). Pool PDA `["pool", coin_mint]`, vaults owned
     by it, x*y=k with `poolFee` and an LP that is never minted (the liquidity is just the vault balances owned by the
     program, so it is "locked" by construction). Fees accrue in the vaults: track fee growth separately
     (`fee_coin_accrued`, `fee_ptoken_accrued` counters incremented per swap) so `collect` pays only fees, not
     principal. Spot price = `vault_p / vault_coin`. Full range in v4 is equivalent to a plain constant product, so
     the economics match.
  2. **Raydium CPMM / Meteora DAMM v2** via CPI, with the LP tokens burned or locked (Raydium's LP-lock program
     returns a fee-claim NFT, and Meteora has position NFTs with permanent lock plus fee claim). Risk: **pool
     front-running**. Raydium CPMM pool addresses are PDAs of (amm_config, mint0, mint1), so anyone can create the pool
     first at a bad price, which is the exact attack `beforeInitialize` prevents. Mitigations: create the pool with a
     custom config or a permissioned pool type, or create at launch time with the pool in a locked state. That is
     messy. Option 1 avoids it.
- `beforeInitialize` hook and CREATE2 salt mining for hook flags -> not needed with our own pool (the pool PDA can only
  be created by our program's `graduate`).
- The unlock/callback flash-accounting pattern -> direct CPI transfers.
- `collect` sweeping the whole balance -> on Solana per-coin pool vaults make this naturally per-coin. Do not sweep a
  shared account.
- `sqrt`, ticks and Q96 are unnecessary with a CPMM. If staying v4-like (concentrated liquidity), port to Orca
  Whirlpools or Raydium CLMM with a full-range position, and accept the same front-running caveat.

---

## 10. Router

**Purpose.** Gives USDG <-> coin in one transaction across the exchange (USDG <-> pToken) and the curve or graduated
pool (pToken <-> coin). It emits a single `Swap` event per trade for indexers and holds no balance between calls.

### State
Immutables `usdg`, `exchange`, `poolManager`. No storage. Error `QuoteResult(uint256)` is used for revert-based
quoting.

### Flows
**buy(curve, usdgIn, minCoins, to, q, sig)**, nonReentrant:
1. `p = curve.pToken()`; pull `usdgIn` USDG from the user to the router; approve the exchange for `usdgIn`.
2. `pOut = exchange.mint(p, usdgIn, minOut = 0, to = router, q, sig)`. All exchange checks apply (quote, band, risk,
   halted). On a settled or pending-$1 market the quote is ignored.
3. `_buy(curve, p, pOut, minCoins, to)`:
   - if `curve.graduated()`: `coinsOut = _swap(curve, p, pOut, to)` (exact-in swap in the v4 pool, coins straight to
     `to`)
   - else: approve the curve for `pOut`; `(coinsOut, pRefund) = curve.buy(pOut, 0, to)`. If `pRefund > 0 &&
     curve.graduated()` (this buy sold out and graduation succeeded), `coinsOut += _swap(curve, p, pRefund, to)` and
     `pRefund = 0`.
   - `coinsOut < minCoins` -> `Slippage(coinsOut, minCoins)`
   - if `pRefund > 0` (sold out but graduation failed), `p.safeTransfer(to, pRefund)`. The buyer gets the leftover
     pToken, which is redeemable.
4. emit `Swap(coin, to, curve, isBuy = true, usdg = usdgIn, coins = coinsOut, pTokens = pOut - pRefund, pTokenRefund =
   pRefund)`.
   - Note: the trader in the event is `to`, not `msg.sender`. Slippage is only on coins; the pToken mint uses minOut 0.

**sell(curve, coinsIn, minUsdg, to, q, sig)**, nonReentrant:
1. `_sellCoins`: pull `coinsIn` coins from the user to the router; if graduated, `pOut = _swap(curve, coin, coinsIn,
   router)`; else approve and `pOut = curve.sell(coinsIn, 0, router)`.
2. `usdgOut = exchange.redeem(p, pOut, minUsdg, to, q, sig)`. The router is the burner of its own pToken. The quote's
   `maxAmount` must cover `pOut`, which the client must predict. Any part delayed or queued still counts in `usdgOut`
   (the return value is `out`, the total owed), so slippage counts USDG that may arrive an hour later (accepted).
3. emit `Swap(coin, to, curve, false, usdgOut, coinsIn, pOut, 0)`.

**sellForShares(curve, coinsIn, minShares, to)**, nonReentrant: `_sellCoins`; `pOut < minShares` -> `Slippage`;
transfer pToken to `to`; emit `Swap(coin, to, curve, false, 0, coinsIn, pOut, 0)`. No quote and no exchange call, so
it works even when halted.

**quotePool(curve, pTokenIn, amountIn)**: not a view. It runs `unlock` with `quoteOnly = true`; the callback swaps,
then reverts `QuoteResult(out)`; the outer function catches it and decodes it (any other error is re-raised). Use via
eth_call.

**unlockCallback(data)**: only the poolManager (`OnlyPoolManager`). It decodes `(key, zeroForOne, amountIn, to,
quoteOnly)`; swaps exact-in (`amountSpecified = -amountIn`, price limit `MIN_SQRT_PRICE+1` / `MAX_SQRT_PRICE-1`, i.e.
no limit); `out = dOut`; if quoteOnly, revert `QuoteResult(out)`; else sync/transfer in/settle, take out to `to`;
return `abi.encode(out)`. Pool-path slippage is enforced only by the caller (`minCoins` / `minUsdg` / `minShares`
downstream).

### Security notes
- The router trusts whatever `curve` the caller passes (`curve.pToken()`, `curve.coin()`, `curve.graduator()`). A fake
  curve can only hurt the caller. The harness checks that "Router always ends empty" and runs fake-curve attacks.
  Tokens sent to the Router by mistake can be taken by anyone.

### Solana redesign
- Simplest: make the router an instruction in the curve/launch program (or a thin program) that CPIs `exchange::mint`
  with **the user as the USDG authority and the user's pToken ATA as the destination**, then `curve::buy` from the
  user's pToken ATA. User signer privileges pass through CPI, so no router-owned transit account is needed (that
  preserves "router holds nothing"). The refund is then just the pToken left in the user's ATA, with no transfer.
- `Swap` event via `emit_cpi!` from the router.
- The sell-out -> graduate -> pool remainder in one transaction requires passing the pool accounts (or the accounts to
  create it) in the buy, which is many accounts. Use an Address Lookup Table. Alternatively, when the remainder is
  above 0 and the curve is not yet graduated, refund the pToken and let the client follow with graduate + a pool swap.
- `quotePool` revert trick -> `simulateTransaction` of the swap instruction, or a read-only instruction that computes
  the CPMM output and returns it via `set_return_data`. With our own CPMM the output is closed-form, so the client
  can compute it.
- `maxAmount` of a SELL quote vs a `pOut` that is unknown until the curve sell executes: the same as on EVM. The client
  pre-computes it and asks for a quote with headroom.

---

## 11. PriceFeed

**Purpose.** Display-only on-chain posting of each market's Polymarket mid, bid and ask (6dp) with a timestamp.
Nothing trades on it.

### State
`ONE = 1e6`; `prices[positionId] = Price{uint64 mid, bid, ask, updatedAt}`; `poster` address (ctor, setPoster);
Ownable2Step owner. Renounce is **not** disabled here.

### Functions
- `setPoster(p)`, onlyOwner; emit `PosterSet`.
- `post(ids[], mids[], bids[], asks[])`: the poster or owner (`NotPoster`); lengths mismatch -> `LengthMismatch`; per
  entry: `mid > ONE || bid > mid || (ask != 0 && ask < mid) || ask > ONE` -> `BadPrice(id)` (this reverts the whole
  batch); store with `updatedAt = now`; emit `PricePosted(id, mid, bid, ask)`.
- `latest(id) view` -> (mid, updatedAt).

### Solana redesign
PDA `["price", position_id]` per market, or one zero-copy account holding a fixed array of (position_id, mid, bid,
ask, ts). Posting costs fees per transaction, so consider dropping it on Solana and exposing prices off-chain only.

---

## 12. Cross-cutting EVM -> Solana items

| EVM mechanism | Where | Solana equivalent |
|---|---|---|
| EIP-712 typed data + `ECDSA.recover` (secp256k1, low-s) | Oracle | ed25519 precompile instruction + Instructions-sysvar introspection (recommended, new pricer key type), or `secp256k1_recover` syscall + keccak to keep the EIP-712 digest. Bind the domain to the program id and cluster. |
| `quoteFilled[digest]` mapping | Exchange | `QuoteFill` PDA `["fill", digest]`, `init_if_needed` (the taker pays rent), closable after expiry. The digest is the message hash or the sig bytes (for ed25519, hash the message). |
| CREATE2 pToken at a predictable address | Exchange | Mint PDA `["ptoken", position_id]`. |
| CREATE2 salt mining for v4 hook flags (0x2000) | Graduator | Not needed: our own pool program, with the pool PDA creatable only by `graduate`. |
| Uniswap v4 pool, unlock/callback, flash accounting, full-range position, locked LP | Graduator, Router | Built-in CPMM with program-owned vaults (LP locked by construction) and separate fee counters; or Raydium/Meteora CPI with LP burn or lock (front-running risk at pool creation). |
| `beforeInitialize` hook | Graduator | PDA ownership: only our program creates the pool. |
| try/catch (graduation, `_tryTransfer`, `_tryRedeem`) | Curve, Exchange, FeeVault | No catch for a CPI failure. Graduation becomes a separate crank. Transfers pre-check the destination account state (frozen, missing, wrong mint) and the USDG mint's pause state, then set aside or skip. |
| `gasleft() < GRADUATION_GAS` | Curve | Not available; moot when graduation is a crank. |
| ERC-20 approve / transferFrom / forceApprove | everywhere | User signs; CPI `transfer_checked` with the user authority. Program-owned vaults sign with PDA seeds. |
| ReentrancyGuard | Exchange, Curve, Router, FeeVault, Graduator | Not needed (CPI reentrancy is forbidden except self-recursion). Keep CEI ordering. |
| `msg.sender` roles, Ownable2Step | all | `Signer` + `has_one` constraints on config PDAs; a `pending_owner` / `accept_owner` pair. |
| Coin `_update` hook -> `FeeVault.onTransfer` | Coin, FeeVault | Token-2022 TransferHook (holder PDAs must pre-exist; the hook cannot pay rent; AMM support varies) or an off-chain merkle distribution. **Needs a product decision.** |
| 18-decimal coin, 1e27 supply | Coin, Curve, FeeVault | u64 SPL limit: use 6 or 9 decimals; rescale `MIN_SUPPLY`, `ACC` and `MAX_ACC`. |
| uint256 math | Exchange risk (12dp), FeeVault acc (1e36) | u128 for the exchange. U256 or rescaled ACC for the FeeVault accumulator. |
| Dynamic arrays (`claims`, `delayed`, `curves`) | Exchange, Factory | Index-seeded PDAs plus head/tail counters in config; close on consume (rent refunds). |
| `mapping(hour => amount)` | Exchange | Two slots (cur, prev). |
| Transfer to `0xdEaD` | Curve, FeeVault | SPL `burn`. |
| `balanceOf(this)` (freeFloat, absorb, collect) | Exchange, Graduator | Read the vault token-account `amount` passed as an account (constrain its address). |
| `block.timestamp` | all | `Clock::unix_timestamp` (i64; stake-weighted and can drift seconds from wall time, so test the 30s quote life and the 1h delays against it). |
| Events | all | `emit_cpi!` (self-CPI event) for reliable indexing. |
| `rescue` | Exchange | `rescue` for non-USDG, non-pToken token accounts owned by the exchange PDA. |
| USDG freeze / pause semantics | Exchange | Check Solana USDG's Token-2022 extensions (Pausable, DefaultAccountState frozen, PermanentDelegate). A permanent delegate can seize the float, which is a new trust assumption to document. |
| Account limits | Router buy across graduation, launch, claimFor | Address Lookup Tables; split into multiple instructions; 1232-byte tx size; about 64 accounts with an ALT. |
