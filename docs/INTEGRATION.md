# Polypad integration guide

For trading terminals, screeners and data providers (GMGN, Axiom, DexScreener,
Bitquery, ...) that want to list Polypad coins: new launches, trades, prices,
holders and graduation, and to let their users buy and sell.

Chain: **Robinhood Chain** (EVM, chain id `4663`). Quote currency for users:
**USDG** (`0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168`, 6 decimals).

> Launch contracts, deployed at block 73,393,714 and verified on Sourcify.

## What a Polypad coin is

A Polypad coin is a memecoin launched on a bonding curve, like a Pons or
pump.fun coin, with one difference: the curve is paired with a **Polymarket
outcome share** instead of ETH. A coin on "Will Spain win?" rises with buying
and moves with Spain's odds, and at resolution it is worth its curve's shares
times the payout ($1 or $0 each).

- Supply: 1,000,000,000 coins (18 decimals), all minted into the curve.
- The curve's quote asset is a **pToken**: a 1:1 claim on one Polymarket share
  (6 decimals), held by Polypad's desk on Polymarket.
- Users trade with **USDG** through the **Router**, which converts USDG to
  pToken at a signed live price and trades the curve in the same transaction.

## Contracts

| Contract | Address | Role |
| --- | --- | --- |
| LaunchFactory | `0x134BBE6a933337b12f4FA6AD6996e7052de8E408` | Creates coins; emits `Launched` |
| Router | `0x12d6eDD87bAf5037D00701cf0803569AbcaDe621` | USDG in and out, curve or pool; emits `Swap` with USDG amounts |
| PExchange | `0x6C245EBCee258A5Ac258c430c081F9736e832eAC` | USDG ⇄ pToken at signed prices |
| PriceOracle | `0x838954F170F8b331aeD5406Df28cd7300Abb8DEA` | Verifies signed prices, posts on-chain prices |
| Graduator | `0xF233B8cB1DA715bC321DC83f809d260e5F0Ce000` | Creates and owns each graduated coin's Uniswap v4 pool; the pools' hook |
| PoolManager | `0x8366a39CC670B4001A1121B8F6A443A643e40951` | Uniswap v4 (Robinhood Chain) |

Each launch adds a **Coin** (ERC-20) and a **BondingCurve**. Each Polymarket
outcome has one **pToken**, shared by every coin on that outcome.

## Events

### New coin — LaunchFactory

```solidity
event Launched(
    uint256 indexed positionId, // Polymarket CLOB token id of the outcome
    address indexed creator,
    address coin,
    address curve,
    address pToken,
    uint256 launchPrice,        // USDG per share at launch, 6 decimals
    uint256 phantom             // curve's virtual pToken reserve, 6 decimals
);
// topic0 0xa671a0a22ea77018ebc904e165cc9077bf00f89927f3157081cbc5d2c2676cb2
```

Start watching `curve` for trades and `coin` for transfers from here.

### Trades in USDG — Router (use this for price and volume)

```solidity
event Swap(
    address indexed coin,
    address indexed trader,
    address indexed curve,
    bool isBuy,
    uint256 usdg,          // USDG paid or received, 6 decimals
    uint256 coins,         // coins bought or sold, 18 decimals
    uint256 pTokens,       // shares into or out of the curve, 6 decimals
    uint256 pTokenRefund,  // shares returned when a buy hit the end of the curve
    bool posted            // priced by the on-chain posted price (true) or a signed quote
);
// topic0 = keccak256("Swap(address,address,address,bool,uint256,uint256,uint256,uint256,bool)")
```

Every trade made with USDG, by any path, is one `Swap` from the Router: watch
**one address** for all trades of all coins. Price and volume come straight
from it; no knowledge of pTokens is needed.

### All trades — BondingCurve

```solidity
event Buy(address indexed buyer, address indexed to, uint256 quoteIn, uint256 coinsOut, uint256 fee, uint256 refund);
// topic0 0x75a85e7be0265abefef113ee168a0d751385a985c3a37920ae97ae192d2eadb4
event Sell(address indexed seller, address indexed to, uint256 coinsIn, uint256 quoteOut, uint256 fee);
// topic0 0x01fbb57444511e3de5b26ac09ad6bec45c3f9a1e59dd4a0f2b13a240d18476ce
event SoldOut();
// topic0 0x52df9fe5b9c9a7b0b4fdc2c9f89387959e35e4209c2a8d133a2b8165edad2a04
```

Amounts here are in pToken (6 decimals). A Router trade emits both a curve event
and a Router `Swap` in the same transaction; count it once (by the `Swap`). A curve event without a Router event is a direct pToken trade; price it
with the coin's last USD price, or with `GET /v1/coins/<coin>` below.

### Holders

Standard ERC-20 `Transfer` on each coin. The curve address holds the unsold
supply and should be excluded from holder counts.

## Pricing

**From a Router `Swap`** (the common case):

```
price (USDG per coin) = usdg / coins     (USDG at 6 decimals, coins at 18)
```

**Spot price at any moment** (moves with the Polymarket odds between trades):

```
curve.spotPrice()      pToken per whole coin, 6 decimals
× share price          the live Polymarket midpoint of the outcome
= USDG per coin
```

or simply `GET /v1/coins/<coin>` → `priceUsd`, pushed live over websocket.

**Market cap** = price × 1,000,000,000.

### Worked example (mainnet)

Buy of $HOLD, tx `0x1c622affe6cc3a88d659d8f30007f8dc4a6f7383a699f68c051b398a31eda4c5`:

```
usdg 2000000, coins 796470313943365450194364
price = 2.000000 / 796470.313943365 = $0.00000251 per coin
mcap  = $2,511
```
(The pilot Router emitted this as `Bought`; production emits `Swap`.)

## Graduation

When the curve's sellable supply runs out (about $6,000 raised at launch odds,
around a $29k market cap) the coin **graduates in the same transaction** into a
standard Uniswap v4 pool against its pToken:

```solidity
// Graduator
event Graduated(address indexed coin, address indexed pToken, bytes32 indexed poolId,
                uint256 coins, uint256 pTokens, uint128 liquidity);
```

- Pool key: `currency0/1` = the coin and its pToken sorted by address, `fee`
  10000 (1%), `tickSpacing` 200, `hooks` = the Graduator. `Graduator.poolKey(coin)`
  returns it.
- One full-range position, owned by the Graduator and never removed: the
  liquidity is locked. Fees go 70% to the creator, 30% to the platform.
- The pool opens at the curve's final price; unused reserve coins are sent to
  `0x…dEaD`.
- After graduation the curve no longer trades. Trades are ordinary v4 `Swap`
  events on the PoolManager for that `poolId`; Router trades still emit the
  Router `Swap` with USDG amounts, and `curve.spotPrice()` reads the pool.
- The Router's `buy` / `sell` / `buyPosted` / `sellPosted` work unchanged: they
  route to the pool once the coin has graduated. `Router.quotePool(curve,
  pTokenIn, amount)` (call it with `eth_call`) quotes a pool trade.

## Letting users trade

Two ways; pick either.

### 1. Plain contract calls (no API)

```solidity
// Buy: approve USDG to the Router first.
router.buyPosted(address curve, uint256 usdgIn, uint256 minCoins, address to)
// Sell: approve the coin to the Router first.
router.sellPosted(address curve, uint256 coinsIn, uint256 minUsdg, address to)
```

Priced at the price Polypad posts on chain for the coin's market
(`PriceOracle.posted(positionId)`), plus a 1.5% spread each way, up to $500 per
trade and $2,000 per market per block. Preview the output with
`exchange.mintPostedOut(positionId, usdgIn)` then `curve.quoteBuy(...)`, or
`curve.quoteSell(coins)` then `exchange.redeemPostedOut(positionId, pTokens)`.
The posted path pauses itself for a few seconds after a big odds move and
whenever the price poster is not running; trades then revert and can be retried.

### 2. Our swap API (best price, any size)

A buy or sell at a fresh signed price from the live Polymarket book, built by
our API the way Jupiter or 0x quotes work:

```
GET https://api.polypad.trade/v1/swap
    ?coin=0x…&side=buy|sell
    &amount=<USDG base units for buy, coin base units for sell>
    &taker=0x…&slippageBps=100
```

```json
{
  "expectedOut": "796470313943365450194364",
  "minOut": "788505610803931795692420",
  "validUntil": 1790452040,
  "approval": { "to": "0x5fc5…1d168", "data": "0x095ea7b3…", "value": "0" },
  "tx": { "to": "0x12d6…E621", "data": "0x00be6a11…", "value": "0", "chainId": 4663 }
}
```

Send `approval` first when present, then `tx`, within `validUntil` (about 10
seconds). Errors come back as `{"error": "..."}` with HTTP 4xx: the market is
paused before its end date, the trade is too large for the Polymarket book, the
odds are moving fast, and so on.

Selling a coin always works, including after buys pause for the market's end
date. After the market resolves YES the coin keeps trading both ways: the
pToken is then worth $1 and mints and redeems at $1 with no quote needed
(0.25% in, 0.5% out); after NO it is worth $0 and only sells remain.

If the exchange's USDG float is ever short, a sell still goes through: the
seller is owed the exact amount (`PExchange.Queued(ticket, to, amount)`) and is
paid automatically, oldest first, as the float refills (usually within a
minute; `ClaimPaid`).

## Contract source

Every Polypad contract is verified on Sourcify (shown on Blockscout), including
each new coin, curve and pToken within minutes of its launch.

## Coin metadata

`Coin.metadataURI()` points to a JSON document in the pump.fun style:

```json
{ "name": "...", "symbol": "...", "image": "ipfs://...", "description": "...",
  "twitter": "...", "telegram": "...", "website": "..." }
```

`GET /v1/coins/<coin>` also returns the Polymarket market (question, outcome,
end date, odds) for display.

## Contact

integrations@polypad.trade
