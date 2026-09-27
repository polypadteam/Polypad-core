# Polypad fees

Every Polypad coin trades against shares of a real Polymarket outcome. A trade
therefore has two parts, and each has a fee:

1. **The market layer.** Your USDG becomes shares of the outcome (a buy), or
   shares become USDG (a sell), at the Polypad exchange.
2. **The token layer.** The shares buy or sell the coin, on its bonding curve
   while it launches, and in its Uniswap v4 pool after it graduates.

Trades from the Polypad site, and from terminals and wallets through our
Router, go through both layers in one transaction.

## Per trade

| | On the curve (launch) | In the pool (after graduation) |
| --- | --- | --- |
| Token fee | **1.4%** | **1.0%** |
| of which to the creator side | 0.7% | 0.5% |
| of which to Polypad | 0.7% | 0.5% |
| Market spread (USDG ⇄ shares) | 0.25% | 0.25% |
| **Total per buy or sell** | **1.65%** | **1.25%** |
| Round trip (buy, then sell) | about 3.3% | about 2.5% |

- The **token fee** is taken from the share side of each trade.
- The **market spread** is charged when USDG is converted to shares or back.
  It is the exchange's margin over the live Polymarket price and covers the
  cost of holding the real shares behind every coin.
- **Launching a coin is free.** The creator pays only network gas.

### Worked example

A $100 buy of a coin still on its curve:

1. $100 of USDG becomes about $99.75 of outcome shares (0.25% spread).
2. 1.4% of those shares, about $1.40, is the token fee: about $0.70 to the
   creator side and $0.70 to Polypad.
3. The remaining $98.35 of shares buys the coin.

## Other fees

| When | Fee |
| --- | --- |
| Cashing out shares after the market has resolved | 0.5% (instead of the spread) |
| Trading at the on-chain posted price (no signed quote, e.g. a plain contract call) | 1.5% spread instead of 0.25% |
| Claiming creator fees or holder rewards | none (network gas only) |

## Creator fees

The creator side of every fee goes to the FeeVault contract, paid in the coin's
market shares (and, for pool sells, in the coin itself).

- **At launch the creator chooses how much of it goes to holders**, from 0% to
  100%. The choice is permanent, so holders can rely on it.
- **The creator's part** can be claimed any time, as shares or converted to
  USDG in the same transaction.
- **The holders' part** is paid to everyone holding the coin, wherever they
  bought it (our site, a terminal, the pool directly), in proportion to their
  balance and for the time they held it. It is paid out automatically every
  hour, in the coin's market shares, once a holder is owed $1 or more; holders
  can also claim at any time. The holders' part of fees paid in the coin itself
  is burned, which every holder shares.

## Fees are fixed per coin

Each coin keeps the fees it launched with, for its whole life, on the curve and
in its pool. Polypad can change the fees for **future** launches only, and only
within limits set in the contracts:

| | Limit |
| --- | --- |
| Curve fee | at most 2% |
| Pool fee | at most 1.5% |
| Creator side's share of either fee | between 30% and 80% |

The market spread is an exchange setting, at most 5%.

## Checking the live numbers

- `GET /v1/fees`: the fees new launches get, and the exchange spreads.
- `GET /v1/coins/<address>`: `fees` is that coin's own schedule, with
  `activePct` the fee it charges now.
- On chain: `BondingCurve.feeBps()`, `creatorShareBps()`, `poolFee()` and
  `poolCreatorShareBps()` for a coin; `LaunchFactory.fees()` for new launches;
  `PExchange.buySpreadBps()` and `sellSpreadBps()` for the spread.
