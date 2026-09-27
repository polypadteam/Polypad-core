# Polypad Core

Smart contracts for [Polypad](https://polypad.trade): memecoins on Robinhood Chain,
each paired with a live Polymarket outcome.

A Polypad coin launches on a bonding curve like any memecoin, but its curve is
priced in **shares of a real Polymarket outcome** instead of ETH. Buying pushes
the coin up the curve; the market's odds move the value of every share on it.
When the market resolves, each share pays $1 or $0, and the coin with it.

## How it fits together

```
user USDG ──Router──> PExchange mints pToken (1 pToken = 1 real Polymarket share, held by the desk)
                          │
                          └──> BondingCurve (coin / pToken) ──> coins to the user
```

| Contract | Role |
| --- | --- |
| `LaunchFactory` | Launches a coin: deploys the `Coin` (1B supply) and its `BondingCurve` |
| `BondingCurve` | Constant-product curve over a virtual reserve; trades the coin against the market's pToken; fees fixed at launch (1.4% on the curve, 1% in the pool, half to the creator side) — see [docs/FEES.md](docs/FEES.md) |
| `PToken` | One per Polymarket outcome: a 1:1 claim on a real share |
| `PExchange` | USDG ⇄ pToken at a verified price; holds the float; caps unbacked supply per market and outflow per hour |
| `PriceOracle` | Verifies signed quotes, holds posted prices, and each market's pause and settlement |
| `Router` | One-transaction USDG buys and sells; emits one `Swap` per trade |
| `Graduator` | Moves a sold-out curve into a Uniswap v4 pool it owns forever (locked liquidity); the pools' only hook |
| `FeeVault` | Receives the creator side of every fee: the creator's claimable balance, and an opt-in dividend streamed to the coin's holders |

### Prices

- **Signed quotes.** Polypad's pricer reads the live Polymarket order book and
  signs a price for one trade, valid for seconds. The quote rides in the user's
  own transaction and `PriceOracle.verify` checks it.
- **Posted prices.** Polypad posts each market's midpoint on chain when it
  moves, so any contract can trade with plain calls (`Router.buyPosted` /
  `sellPosted`) at a wider spread and capped size. Posted prices are usable only
  while the poster is alive, and a big jump halts them briefly. **Closed at
  launch** (`postedMaxTrade = 0`): a one-step trade at a posted price can be
  raced by anyone who sees Polymarket move first; it reopens with a two-step fill.
- **Settlement.** The keeper records each market's payout from Polymarket's
  on-chain result; it takes effect an hour later (`SETTLE_DELAY`), and the owner
  can cancel a wrong one meanwhile, so one hot key cannot settle a market at $1
  and redeem the float.

### Safety

- Unbacked pToken per market is capped until the desk reports the real shares.
- USDG leaving through redemptions is capped per hour; the keeper can halt the
  exchange, only the owner resumes it.
- A new signer, poster or bridge address takes effect only after 2 days;
  revoking the signer or poster is immediate.
- `rescue` can never move the float or a pToken.
- Selling always works while a market is live, including after buys pause before
  the end date. After resolution, pTokens pay the market's payout.

## Integrating

See [docs/INTEGRATION.md](docs/INTEGRATION.md) for events, pricing, the swap API
and plain on-chain trading. ABIs are in [`abi/`](abi) (JSON, and TypeScript in
`abi/index.ts`). Deployed addresses are in [`deployments/`](deployments).

## Build and test

```sh
git clone --recurse-submodules git@github.com:polypadteam/Polypad-core.git
cd Polypad-core
forge build
forge test
```

Requires [Foundry](https://book.getfoundry.sh). Solidity 0.8.28, optimizer 1,000,000 runs.

## License

The contracts in `src/` are licensed under the Business Source License 1.1
(see [LICENSE](LICENSE)). ABIs, docs, tests and scripts are MIT (see
[LICENSE-MIT](LICENSE-MIT)).
