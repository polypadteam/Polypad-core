# Meteora DBC → DAMM v2 for `polypad_launch`

Research as of 2026-10-13, for Phase 2 of [../SOLANA.md](../SOLANA.md) (decision D1),
corrected after the Phase 1 review and after building `polypad_launch` against the real
programs. Everything below was read from source, pinned to these commits:

| Repo | Commit | Version |
|---|---|---|
| [MeteoraAg/dynamic-bonding-curve](https://github.com/MeteoraAg/dynamic-bonding-curve) | `f552f20` (2026-09-09) | program 0.2.1, anchor-lang 1.0.2 |
| [MeteoraAg/damm-v2](https://github.com/MeteoraAg/damm-v2) | `a85c926` (2026-09-08) | cp-amm 0.2.4, anchor-lang 1.0.2 |
| [MeteoraAg/dynamic-bonding-curve-sdk](https://github.com/MeteoraAg/dynamic-bonding-curve-sdk) | `a28b723` | IDL 0.2.1 |
| [MeteoraAg/damm-v2-sdk](https://github.com/MeteoraAg/damm-v2-sdk) | `79ebbfe` | IDL cp_amm 0.2.5: identical to the 0.2.4 source apart from the version field |

Paths below are relative to those repos. DBC program sources sit in
`programs/dynamic-bonding-curve/src/` (written `dbc/` below); DAMM v2 sources sit in
`programs/cp-amm/src/` (written `cp-amm/`). Our program is
`solana/programs/polypad_launch`; its tests run the mainnet DBC, DAMM v2 and Metaplex
binaries in LiteSVM ([§8](#8-what-the-build-proved)).

## Decisions

| # | Decision | Why |
|---|---|---|
| M1 | **One DBC config per coin**, created by `polypad_launch::launch` through CPI. The config is a PDA of our program, `["dbc_config", coin_mint]`, so only `launch` can create it. | A DBC config fixes the quote mint, start price, curve and migration threshold. Our curve depends on the market (quote = that market's pToken) and on the launch price, so configs cannot be shared. Our program derives every parameter from the verified quote, so a client can never pass a hand-made curve. A DBC config is **reusable by anyone** once it exists: anyone can open more pools on it with fresh mints (§4). |
| M2 | **`launch` CPIs `create_config` only. The pool creation (`initialize_virtual_pool_with_spl_token`) and the creator's first buy (`swap2`) are top-level DBC instructions the API puts after it.** The coin's mint keypair signs `launch`. | DBC charges the creator's first buy the curve fee instead of the anti-sniper schedule's start only when the pool creation and that swap are both top-level DBC instructions of one transaction (§6). A CPI'd pool creation can never qualify. The mint's signature means nobody can make a config for someone else's mint, and only the mint's holder can create the pool the record names. |
| M3 | **All curve and pool fees go to the partner, a fee-vault PDA of our program.** `creator_trading_fee_percentage = 0`; post-migration LP is 100% `partner_permanent_locked`. Our vault splits creator/platform per coin, as the EVM FeeVault does. | One place holds the split, the holders' share (D3) and payee changes. Meteora's own creator role then carries no money, so the user can be the DBC creator (terminals show the real dev wallet) without being able to claim fees around our vault. |
| M4 | **Curve fee: 1.4% base** (`cliff_fee_numerator = 14_000_000` at the end of the schedule, M5), collected in pToken only. **Pool fee: 1% base plus a dynamic (volatility) fee of up to about 0.2%, through DAMM v2 static config `Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp`** (`migration_fee_option = 2`, FixedBps100). That config collects in token B (pToken) only. | The trader pays what the EVM charges on the curve. Meteora's 20% protocol share comes out of it (see [Fees](#3-fees)). FixedBps100 is a Meteora-made static config with the fee mode we want. Its dynamic fee is on (§1), so the pool fee moves between 1.0% and about 1.2% with volatility; the extra goes to the LP, which is our vault. If a flat fee matters, the customizable path (`A8gM…`, `migration_fee_option = 6`, `migrated_pool_fee.dynamic_fee = 0`) gives one. Accepted for v1. |
| M5 | **Anti-sniper fee schedule: linear from 25.4% to the coin's curve fee over 60 s** (`base_fee_mode = 0`, `cliff = fee + 240_000_000`, 60 periods of 1 s, `reduction_factor = 4_000_000`). `enable_first_swap_with_min_fee = true`, so the creator's buy in the pool-creation transaction pays the curve fee. | Decided for v1. It taxes buys and sells alike, so a sniper's flip pays it twice. Meteora takes 20% of the high early fees too. |
| M6 | **Our keeper migrates every graduated coin with `polypad_launch::graduate`, which CPIs DBC `migration_damm_v2` itself, passing the pinned `Hv8L…` config.** If anyone migrated first (DBC allows it), the keeper's `graduate` only records the result, naming the first position of DBC's migration transaction; only the keeper may record a migration it did not run. Either way it requires the fee vault's position to hold, permanently locked, exactly the liquidity the migration creates (a number fixed at launch), never a share of the pool's live liquidity. | Meteora's migration keepers serve only fixed quote mints and thresholds (10 SOL, 750 USDC, 100 TRUMP, 1500 JUP, 750 USD1, 1500 MET, 750 JupUSD, 42000 VIRTUAL), plus Jupiter-verified quotes with an Organic Score above 50 and $750 notional, and they run on mainnet only (docs.meteora.ag/developer-guides/dbc, "Migration Keepers"). A pToken is none of these. Until migration runs, the coin cannot trade at all. The first build compared the position with the pool's live liquidity, so anyone adding liquidity between a third-party migration and `graduate` blocked graduation, and every pool fee of the coin, for good. The second let anyone record a third-party migration, and a twin position handed to the vault could be recorded instead of DBC's (§4). |
| M7 | **CPI bindings come from `declare_program!` over vendored IDL JSON** (`solana/idls/`), not Meteora's crates. | No Meteora crate is published on crates.io. Their program crates pin anchor 1.0.2 and pull heavy dependencies. `declare_program!` is how DBC itself calls DAMM v2 (`libs/damm-v2/src/lib.rs`). Built and working on anchor-lang 1.2.1. |
| M8 | **The canonical coin is identified by its `Launch` record** (`["launch", coin_mint]` under `polypad_launch`), never by `fee_claimer` and not by config address alone, **and only if its pool was opened before the launch quote expired** (`Launch.pool_deadline`). | `create_config` is permissionless and `fee_claimer` is an unsigned pubkey, so anyone can make a DBC config that pays "us" and launch a look-alike; and anyone can open a second pool on our own per-coin config. Only the pool named in a `Launch` record (`Launch.pool`, whose base mint is the record's seed) is a Polypad coin. The indexer and every fee-vault instruction accept only that pool. A pool opened after the deadline sits on a curve sized at a stale price: `collect` and `graduate` refuse it and anyone may close its record (`expire_launch`). Partner fees DBC holds for our vault on pools that are not coins, and the fees of DAMM v2 positions their migrations hand our vault, are swept to the platform by the keeper (`sweep_fees`, `sweep_position_fees`), never in a coin's mint. |

## 1. Availability and program ids

Both programs and both migration configs are live on **devnet and mainnet at the same
addresses**. I checked each with `getAccountInfo` on both clusters on 2026-10-13.

| Thing | Address | Devnet | Mainnet |
|---|---|---|---|
| DBC program | `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN` | yes (upgradeable, authority `DHLXnJdA…`) | yes (upgradeable, authority `JADaUV8k…`) |
| DAMM v2 (cp-amm) | `cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG` | yes | yes (authority `JADaUV8k…`) |
| DBC pool authority (PDA) | `FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM` | holds 99 SOL | holds 69 SOL |
| DBC event authority (PDA) | `8Ks12pbrD6PXxfty1hVQiE9sc289zgU1zHkvXhrSdriF` | | |
| DAMM v2 pool authority (PDA) | `HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC` | | |
| DAMM v2 event authority (PDA) | `3rmHSu74h1ZcmAisVcWerTCiRDQbUrBKmcwptYGjHfet` | | |
| DAMM v2 migration config, 1% (FixedBps100) | `Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp` | yes | yes |
| DAMM v2 migration config, customizable | `A8gMrEPJkacWkcb3DGwtJwTe16HktSEfvwtuDh2MCtck` | yes | yes |
| Metaplex Token Metadata | `metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s` | | |
| Meteora protocol-fee wrapper | `pFee3tb7qh5z53jRF4PbLwmNd148Q8ypLNZbqsMeinA` | **absent** | yes |

**Devnet and mainnet run different builds.** ProgramData last-deploy slots:

| | Mainnet | Devnet |
|---|---|---|
| DBC | slot 445503633, 2026-09-09: release 0.2.1 (the pinned commit) | redeployed 2026-10-08 (slot 508712305), after the pinned commit |
| cp-amm | slot 445230614, 2026-09-08: 0.2.4 | redeployed 2026-09-23 |

Devnet DBC very likely runs the unreleased `release_0.2.2` branch (last commit
2026-10-07). Its CHANGELOG deprecates `create_config` in favour of `create_config2`, adds
`PoolConfig.version = 1`, removes `EvtCreateConfig`, changes quote-mint error codes, and
changes the `migration_damm_v2` and `withdraw_leftover` paths. So devnet tests may not
match mainnet 0.2.1, and the vendored 0.2.1 IDL may not match devnet. Consequences:

- The Phase 2 tests run the **mainnet** binaries (dumped with `solana program dump`) in
  LiteSVM. Devnet is for the end-to-end services loop, not for confirming Meteora's math.
- Before mainnet, re-diff the IDL against whatever Meteora releases next and confirm CU
  and behaviour on a Surfpool mainnet fork.
- When 0.2.2 reaches mainnet, `launch` should CPI `create_config2` (passing `None` for
  `transfer_fee_parameters`); `create_config` is only deprecated, not removed, in 0.2.2.

Sources: `dynamic-bonding-curve-sdk/packages/dynamic-bonding-curve/README.md` ("Program
Address", "Graduated DAMM Pool Config Keys") and `src/constants.ts`
(`DAMM_V2_MIGRATION_FEE_ADDRESS`). The DAMM v2 docs say the program id is the same on
mainnet and devnet (docs.meteora.ag/developer-guides/damm-v2/rust-integration/cpi).

I decoded the on-chain `Hv8L…` config (identical on both clusters):
`pool_creator_authority = FhVo3m…` (DBC only), `cliff_fee_numerator = 10_000_000` (1%), no
schedule, **dynamic fee on** (`initialized = 1`, `bin_step = 1`, `filter_period = 10`,
`decay_period = 120`, `reduction_factor = 5000`, `variable_fee_control = 956`,
`max_volatility_accumulator = 14_460_000`: Meteora's standard settings, a dynamic fee
capped at 20% of the base fee, so at most about 0.2% on top of 1%),
`protocol_fee_percent = 20`, `referral_fee_percent = 20`, `activation_type = 1`
(timestamp), `collect_fee_mode = 1` (OnlyB, the quote), `permission = 1`
(`CreatePoolWithoutMintValidation`). DAMM v2 `initialize_pool` copies the config's fees
into the pool (`cp-amm/state/config.rs::to_pool_fees_struct`). The protocol-fee wrapper is
missing on devnet, but only Meteora uses it, so it does not affect us. For manual
migration on either cluster there is <https://migrator.meteora.ag>.

The DBC pool authority must hold SOL: at migration it advances the DAMM v2 rent and the
migrator repays it in the same instruction (`dbc/instructions/migration/flash_rent.rs`).
Meteora keeps it funded on both clusters; our tests fund it as mainnet does.

## 2. Reproducing our curve

### The mapping

EVM (`contracts/src/BondingCurve.sol`, EVM-PORT-SPEC §5): constant product
`(phantom + q) · y = phantom · S`, with 5/7 of supply sellable. Graduation comes at
`q* = 2.5 · phantom = gradUsd / price` shares. The pool is seeded with the whole raise at
the last price, and the remaining 4/49 of supply is burned.

DBC's curve is a list of concentrated-liquidity segments (`dbc/curve.rs`,
`dbc/params/liquidity_distribution.rs`). Prices are `sqrt(quote atoms per base atom)` in
Q64.64. Liquidity is Q64-scaled, and `quote = L·Δ√P / 2^128`. A constant-product curve
over virtual reserves is **exactly one segment**: `L = sqrt(phantom · S)` and
`√P0 = sqrt(phantom / S)`. The curve sells out when √P reaches `3.5 · √P0`, because the
price rises 12.25× as on the EVM.

| DBC field (`ConfigParameters`, `dbc/instructions/partner/create_config/process_create_config.rs`) | Value | Note |
|---|---|---|
| `quote_mint` (account) | the market's pToken mint | Classic SPL is always accepted as a quote mint (`dbc/utils/token.rs::is_supported_quote_mint`), so no token badge is needed. |
| `token_type` | 0 (SPL) | D4 |
| `token_decimal` | 6 | DBC allows 6–9 |
| `sqrt_start_price` | `isqrt(phantom · 2^128 / S)` | U256 math on chain |
| `curve` | `[{ sqrt_price: √P_end, liquidity: isqrt(phantom · S · 2^128) }]` | One segment, with **`√P_end = √P0 + ceil(q* · 2^128 / L)`**: the exact 3.5 · √P0, rounded up. Then the segment holds `q* + 1` (DBC rounds the quote up), so `get_migration_threshold_price` takes its "total > threshold" branch and sets the migration price to `√P0 + floor(q* · 2^128 / L)`. Ending the curve there (not at `MAX_SQRT_PRICE`) keeps DBC's 25% "swap buffer" from minting extra supply (`PoolConfig::get_swap_amount_with_buffer` caps the buffer at the base on the whole curve). |
| `migration_quote_threshold` | `q* = phantom · 5 / 2` (pToken atoms) | The same number as the EVM `target()`. `quote_reserve` counts net-of-fee input (`apply_swap_result`), exactly like `trackedQuote`. |
| `token_supply` | `Some { pre: 1_000_000_000_000_000, post: swap_base + migration_base }` | Fixed supply of 1e9 coins. `post` is the program's own minimum (`minimum_base_supply_without_buffer`), so the burn at migration is `pre − post` (about 4/49). `launch` computes both terms with DBC's own rounding before the CPI, since `post` is an input. |
| `migration_option` | 1 (DAMM v2) | 0 (DAMM v1) is rejected since 0.2.1 |
| `migration_fee_option` | 2 (FixedBps100 → `Hv8L…`) | Requires `migrated_pool_fee` = all zero and `migrated_pool_base_fee_mode = 0`, market-cap scheduler params zero, `compounding_fee_bps = 0` |
| `migration_fee` | `{ fee_percentage: 0, creator_fee_percentage: 0 }` | No cut of the raise. The whole `q*` seeds the pool. |
| `partner_permanent_locked_liquidity_percentage` | 100 | Satisfies the "≥10% locked at day 1" rule (`MIN_LOCKED_LIQUIDITY_BPS`) |
| `partner_liquidity_percentage`, `creator_liquidity_percentage`, `creator_permanent_locked_liquidity_percentage` | 0, 0, 0 | The six LP percentages must sum to 100 |
| `partner_liquidity_vesting_info`, `creator_liquidity_vesting_info`, `locked_vesting` | all zero | |
| `pool_fees.base_fee` | `{ cliff_fee_numerator: fee + 240_000_000, first_factor: 60, second_factor: 1, third_factor: 4_000_000, base_fee_mode: 0 }` | The M5 schedule: linear, 60 one-second periods, ending at `fee` (14,000,000 = 1.4% by default). FEE_DENOMINATOR is 1e9. DBC's minimum is 0.25% and its maximum 99% (`dbc/constants.rs::fee`). |
| `pool_fees.dynamic_fee` | `None` | |
| `collect_fee_mode` | 0 (QuoteToken) | Buy fee taken from pToken input, sell fee from pToken output |
| `activation_type` | 1 (timestamp) | The schedule runs in seconds |
| `creator_trading_fee_percentage` | 0 | M3 |
| `token_update_authority` | 1 (Immutable) | Metadata `is_mutable = false`, as the EVM `metadataURI` is fixed |
| `pool_creation_fee` | 0 | Launch stays free (beyond rent) |
| `enable_first_swap_with_min_fee` | true | M5, §6 |
| `fee_claimer` (account) | our fee-vault PDA | Signs claims through CPI |
| `leftover_receiver` (account) | our fee-vault PDA | Receives rounding dust that is above the burn cap. We burn it (`burn_leftover`). |

What DBC does with this (`process_create_config`, `ConcentratedLiquidity`):

- `migration_sqrt_price` = the price after selling exactly `q*` quote from √P0.
- `swap_base_amount` = the base sold from √P0 to the migration price, which is 5/7 S.
- `migration_base_threshold` = the base that pairs with `q*` in a full-range DAMM v2
  position at the migration price (`get_included_protocol_fee_migration_amounts_1`),
  which is ≈ `q* / P_end` = 10/49 S. That is the EVM's `poolCoins`.
- At migration (`dbc/instructions/migration/dynamic_amm_v2/migrate_damm_v2_initialize_pool.rs`)
  the pool is created at `migration_sqrt_price` over `[MIN_SQRT_PRICE, MAX_SQRT_PRICE]`.
  Full range is equivalent to x·y=k, matching the EVM's full-range v4 position. The
  leftover base is burned up to `pre − post`. Anything above that is claimable by
  `leftover_receiver` through `withdraw_leftover`.

So the sold, pooled and burned shares are 71.43% / 20.41% / 8.16%, identical to the EVM.
**The DBC supply after migration is 918,367,354 coins**: Solana burns for real, where the
EVM sends coins to DEAD.

### Worked examples (gradUsd $6,000; coin and pToken both 6 dp)

These numbers come from a Python replica of DBC's integer math (`create_config` path,
same rounding), and are reproduced exactly by `polypad_launch/src/curve.rs`
(`reproduces_the_doc_table`) and by the real DBC program in the LiteSVM tests.

| | Market at 60c | Market at 5c |
|---|---|---|
| `phantom` (EVM formula `gradUsd·2e6/(price·5)`) | 4,000,000,000 (4,000 shares) | 48,000,000,000 (48,000 shares) |
| `migration_quote_threshold` = q* | **10,000,000,000** (10,000 shares = $6,000) | **120,000,000,000** (120,000 shares = $6,000) |
| `sqrt_start_price` | **36893488147419103** (√0.000004 · 2^64) | **127802791879540120** |
| `curve[0].sqrt_price` | **129127208515966862** | **447309771578390423** |
| `curve[0].liquidity` | **36893488147419103232000000000000** (2e12 · 2^64) | **127802791879540120812585215911708** |
| → `migration_sqrt_price` (computed by DBC) | 129127208515966861 (√P × 3.5) | 447309771578390422 |
| → `swap_base_amount` | 714,285,714,285,715 (71.43%) | 714,285,714,285,715 |
| → `migration_base_threshold` | 204,081,639,440,922 (20.41%) | 204,081,634,611,492 |
| `token_supply.pre` / `.post` | 1,000,000,000,000,000 / **918,367,353,726,637** | 1,000,000,000,000,000 / **918,367,348,897,207** |
| Burned at migration (`pre − post`) | 81,632,646,273,363 (8.16%) | 81,632,651,102,793 |
| Start price | 0.000004 pToken/coin → FDV 4,000 shares = **$2,400** | 0.000048 → 48,000 shares = **$2,400** |
| Graduation price (12.25×) | 0.000049 → FDV **$29,400** | 0.000588 → **$29,400** |
| Gross pToken paid by buyers to graduate (fee 1.4%) | ≈ 10,142 shares | ≈ 121,704 shares |
| Seeded into DAMM v2 after Meteora's 0.2% migration cut | 9,980 shares + 203,673,476 coins | 119,760 shares + 203,673,471 coins |

The quote needed to move from √P0 to the migration price is `q*` plus one atom of
rounding in both cases (the real pool stops at `quote_reserve = q* + 1`, the only
surplus). The dollar FDV at launch is $2,400 for every price, because
`phantom · price = 0.4 · gradUsd`, as on the EVM.

Implementation notes for `launch` (all done in `polypad_launch`):

- `phantom · S · 2^128` reaches ~8e65 at 0.1c (phantom 2.4e12 with gradUsd $6,000), so
  the square roots are taken in U256 (`ruint`, which DBC uses, with a Newton isqrt). L
  stays below 2^128.
- After the CPI, `launch` reads the whole `PoolConfig` back (`meteora::check_config`) and
  requires every field to equal what it asked for or computed: the quote mint, fee
  claimer and leftover receiver; the fee schedule (cliff, periods, frequency, reduction,
  mode) and no dynamic fee; `collect_fee_mode`, `migration_option`,
  `migration_fee_option`, `activation_type`, token type and decimals; the six LP
  percentages, both LP vesting infos and the locked vesting; the migration fee and the
  migrated-pool fee fields; `creator_trading_fee_percentage`, `token_update_authority`,
  `enable_first_swap_with_min_fee`, `pool_creation_fee`; the fixed-supply flag, `pre` and
  `post`; the start price and the one curve segment followed by an empty slot; and DBC's
  computed `swap_base_amount`, `migration_base_threshold`, `migration_sqrt_price` and
  threshold. Any drift (a DBC upgrade changing a layout or a meaning) refuses the launch.
  Only `version` and `quote_token_flag` are not compared: DBC sets them from its release
  and the quote mint, and pinning `version` would stop launches at Meteora's next upgrade
  even with unchanged math. A test flips each compared field of a real config and expects
  the check to fail.
- Buys use `swap2` with `swap_mode = 1` (PartialFill). ExactIn **reverts** if it
  would cross the migration price (`InsufficientLiquidity`). PartialFill stops at the
  migration price and charges only what it used, which is the EVM's refund behaviour
  (`dbc/state/virtual_pool.rs::get_swap_result_from_partial_input`). Sells use ExactOut
  for an exact pToken amount, so the same transaction can redeem it.
- Behaviour that differs from the EVM: once `quote_reserve ≥ threshold` **every swap is
  refused** (`PoolIsCompleted`) until migration. On the EVM, sells stay open while the curve
  is sold out but not graduated. That is why migration must be fast (M6).
- Name/symbol/uri go through Metaplex: **name ≤ 32, symbol ≤ 10, uri ≤ 200 bytes**. The
  EVM allows 64/16/512, so the API and UI must apply the lower limits.
- DBC rounds every fee up (`calculate_fee` and the scheduler); the tests check fees to
  the atom.

## 3. Fees

### What Meteora takes

| Where | Meteora's cut | Source |
|---|---|---|
| DBC curve trades | **20% of the trading fee** (`PROTOCOL_FEE_PERCENT = 20`). If the swap passes a `referral_token_account`, 20% of that 20% goes to the referrer (`HOST_FEE_PERCENT`). | `dbc/constants.rs::fee`, `PoolFeesConfig::get_fee_on_amount` |
| DBC migration | **0.2% of the liquidity migrated**, both tokens (`PROTOCOL_LIQUIDITY_MIGRATION_FEE_BPS = 20`) | `dbc/constants.rs`, `migrate_damm_v2_initialize_pool.rs` |
| DBC surplus | 20% of any quote above the threshold (80% to partner + creator) | `PARTNER_AND_CREATOR_SURPLUS_SHARE = 80`. With PartialFill the surplus is one atom. Not claimed in v1. |
| DBC pool creation fee | 10% of `pool_creation_fee` | We set 0 |
| DAMM v2 trades | **20% of the trading fee** (`cp-amm/constants.rs::fee::PROTOCOL_FEE_PERCENT`, and `protocol_fee_percent = 20` in config `Hv8L…`), 20% of that to a referrer | |

### Our numbers

| | Trader pays | Meteora | Our vault (partner) | Split by our vault (default 50/50) |
|---|---|---|---|---|
| Curve, after the first minute | 1.4% of the pToken side | 0.28% (0.224% net when we pass our referral account) | 1.12% | creator side 0.56% / platform 0.56% (+0.056% referral on swaps we route) |
| Curve, first minute | 25.4% falling to 1.4% (§6) | 20% of it | 80% of it | same split |
| Pool | 1% of the pToken side, plus 0–0.2% dynamic | 20% of it: 0.2–0.24% (less with our referral) | 0.8–0.96% (all LP is our position) | 0.4–0.48% / 0.4–0.48% |

Measured in the tests: a single 1,662-share buy on the graduated pool paid our vault 79 bps
of its input (1% base, a dynamic fee near zero for one trade, less Meteora's 20%).

To keep the EVM's 0.7%/0.7% exactly, the curve fee would have to be 1.75%
(`cliff_fee_numerator = 17_500_000`); likewise 1.25% on the pool, which needs the
customizable path. **Open decision for the user: hold the trader price (recommended,
built) or hold the creator take.** `set_fees` allows 0.25–2% on the curve.

### Claims (all fees arrive in pToken)

| When | Instruction | Signer | Tokens |
|---|---|---|---|
| Curve phase, anytime, also after migration | DBC `claim_trading_fee(max_amount_a, max_amount_b)` | `fee_claimer` = our vault PDA (CPI, `invoke_signed`) | Base fee 0 (quote mode): DBC skips the base transfer when it is 0. Quote fee → `token_b_account`. `token_a_account` may be any token account (no mint constraint in `ix_claim_partner_trading_fee.rs`). We pass the vault's coin ATA, which we need anyway (below). |
| Curve phase | DBC `claim_creator_trading_fee` | creator (user) | Always 0 with `creator_trading_fee_percentage = 0` |
| Pools that are not coins (more pools on a coin's config, look-alike configs naming our vault, an expired launch's pool) | DBC `claim_trading_fee` through **`polypad_launch::sweep_fees`** | keeper; vault PDA signs | Credited to the platform (`Owed` under `["platform_owed", mint, platform]`) in whatever the pool paid, measured as the vault's balance change. Refused for any pool whose base **or quote** mint has a `Launch` record: a coin's fees always go through its split, and nothing is ever owed in a coin's mint (a look-alike config may take a coin as its quote; those fees would land in the vault's coin account, which `burn_leftover` burns). |
| The same pools after anyone migrates them (DBC hands our vault their LP, locked) | DAMM v2 `claim_position_fee` through **`polypad_launch::sweep_position_fees`** | keeper; vault PDA owns the NFT | Credited to the platform as above. Refused when either of the pool's mints has a `Launch` record, so a coin's position goes through `collect`; a late coin's record is closed first (`expire_launch`). |
| After the curve completes | DBC `partner_withdraw_surplus` | vault PDA | One atom; not claimed in v1 |
| After migration | DBC `withdraw_leftover` (permissionless) | anyone | Coin dust above the burn cap (3 atoms in the tests) → the vault's coin ATA (`associated_token::authority = leftover_receiver`); `polypad_launch::burn_leftover` burns it, for graduated coins only (a record made later for a look-alike's mint never graduates, so what the platform is owed in that mint is never burned) |
| After migration | DAMM v2 `claim_position_fee` | vault PDA owns the position NFT account | Token B (pToken) only, because `collect_fee_mode = OnlyB`. Its `token_a_account` must be a separate account from `token_b_account` (Anchor refuses duplicate mutable accounts), hence the vault's coin ATA. |
| Never (zero) | DBC `withdraw_migration_fee`, `creator_withdraw_surplus`, `claim_partner_pool_creation_fee` | | |

`polypad_launch::collect` (permissionless crank, the analogue of `Graduator.collect` and
the curve's `_payFee`) CPIs `claim_trading_fee` and, once graduated, `claim_position_fee`;
measures the pToken delta on the vault's ATA for that market around each claim; and books
the split from the coin's stored `Fees` (EVM rounding: creator side floored from the fee,
holders' part floored from the creator side, the platform takes the rest). Payees and the
platform are owed per `(pToken mint, payee)` (`Owed` PDAs, the EVM `owed[asset][payee]`);
the platform's have their own seed prefix (`["platform_owed", …]`, payees' `["owed", …]`),
so the owner may make the platform any wallet, a creator's included, and `collect` still
credits both sides to separate accounts (the first build shared one seed, and a platform
equal to a payee made that coin's `collect` fail);
the holders' part stays on the coin's record until the keeper moves it to the payout
wallet (D3). The vault's pToken ATA is shared by every coin of a market, so only deltas
count and it is never swept (EVM-PORT-SPEC §9 "Solana redesign"); donations to it are
never booked.

## 4. Front-running, hijacking and migration price

**No one can pre-create either pool, and no one can move the migration price. Anyone can
open extra pools on our configs, which never touch our books.**

- **DBC pool address** = `["pool", config, max(base_mint, quote_mint), min(...)]` under
  DBC (`ix_initialize_virtual_pool_with_spl_token.rs`). Our config PDA does not exist
  before `launch`, and the base mint is a fresh keypair that must sign both `launch` and
  DBC's pool creation (DBC initializes the mint). Nothing exists to front-run, and only
  the mint's holder can create the pool `launch` recorded. If the launch bundle's second
  transaction never lands, the record names a pool that does not exist yet; nobody but the
  creator can create it, so there is nothing to snipe meanwhile.
- **The pool must open before the launch quote expires.** `launch` checks the quote, the
  band and that the market is open, but cannot see the pool (it is created after it). So
  the record keeps `pool_deadline = quote.valid_until` (at most 30 s away). DBC activates a
  pool at creation and stores that time (`activation_point`); `collect` and `graduate`
  refuse a pool opened after the deadline (`PoolTooLate`), and anyone may close such a
  record, or one whose pool never opened, with `expire_launch` (the rent goes back to the
  creator). Without this a creator could launch while the market is open and cheap, keep
  the mint keypair and open the pool days later on a stale curve, after a pause or a
  settlement. A closed coin can never be launched again (its config exists), and its pool,
  if opened, is a look-alike. The indexer should show a coin only once its pool exists
  with `activation_point <= pool_deadline`. Anyone may still complete and migrate a late
  pool (DBC knows no deadline); the coin's config then hands our vault the DAMM v2
  position, locked for good, and `graduate` refuses it. Its fees are not stranded: once
  the record is closed, the keeper claims them for the platform with
  `sweep_position_fees` (tested: a late pool completed, migrated by a stranger, traded,
  swept and withdrawn by the platform).
- **Configs are reusable by anyone.** `initialize_virtual_pool_with_spl_token` accepts any
  existing config (its only constraint is `has_one = quote_mint`). So anyone can open more
  DBC pools on our per-coin config, each with a fresh mint, our `fee_claimer` and their
  own creator, including between the two transactions of a launch bundle. They are
  look-alike coins on an identical curve. What still holds: their quote mint is our
  pToken (DBC enforces it); they have no `Launch` record; `collect` claims only
  `Launch.pool` (the tests try both a foreign config and a second pool on our config and
  are refused); `graduate` accepts only the record's pool. Their partner fees accrue in
  DBC for our vault and are never credited to any creator; the keeper sweeps them to the
  platform with `sweep_fees`, which refuses any pool whose base or quote mint has a
  `Launch` record (tested with a look-alike config quoted in a live coin), and, once
  someone migrates such a pool, the fees of the position its migration hands our vault
  with `sweep_position_fees`. The indexer must show only `Launch.pool` pools.
- **DAMM v2 pool address** = `["pool", damm_config, max(mint), min(mint)]` under cp-amm
  (`ix_initialize_pool.rs`, `…_with_dynamic_config.rs`), so the config used decides the
  address; `meteora::damm_pool_address` takes the config, and `graduate` derives the pool
  from the config account it is given, which must be `Hv8L…`. `Hv8L…` has
  `pool_creator_authority = FhVo3m…` (DBC's PDA), and cp-amm's `handle_initialize_pool`
  requires `config.pool_creator_authority == default || == payer`; DBC passes its pool
  authority as the payer. Only DBC's migration can create a pool under it, and the coin
  mint belongs to exactly one DBC pool, which migrates once.
- **Who picks the DAMM v2 config.** DBC's migration takes it as `remaining_accounts[0]`
  from whoever calls it, and `validate_config_key` checks its properties, not its address:
  for our `migration_fee_option = 2` (FixedBps100) a fee-time-scheduler base fee of
  exactly 100 bps with no schedule, the full price range, no vault config, timestamp
  activation, and `pool_creator_authority` = DBC's pool authority. It does not check the
  dynamic fee or `collect_fee_mode`. cp-amm configs are created only by Meteora's
  operators. On 2026-10-13 mainnet held seven configs only DBC may use
  (`getProgramAccounts` on cp-amm, 328-byte `Config`, authority at offset 40): static
  ones at 0.25%, 0.3%, **1% (`Hv8L…`, the only one)**, 2%, 4% and 6%, and the
  customizable `A8gM…`. So DBC accepts only `Hv8L…` for our coins, and nobody can
  migrate one into another pool first; the tests load the 2% config and `A8gM…` and see
  DBC refuse both (`InvalidMigrationFeeOption`). `graduate` passes `Hv8L…` itself and
  accepts no other. `tests/fixtures/fetch.sh` re-lists the DBC-only configs and warns if
  the set changed: if Meteora ever adds a second flat-1% one, a front-run migration into
  it would leave the coin ungraduated here (its pool fees unclaimable until a program
  upgrade accepts that config), so the keeper must then migrate at once and the program
  should learn the new config.
- **Graduation cannot be blocked.** Anyone can call DBC's migration, and anyone can open a
  DAMM v2 position on the new pool and add liquidity. `graduate` therefore never compares
  with the pool's liquidity: it requires the fee vault's position (NFT held by the vault,
  `pool` = this pool, nothing unlocked or vesting) to hold exactly
  `migrated_liquidity(threshold, migration_sqrt_price, fee_bps)`: the raise less
  Meteora's migration cut (`protocol_liquidity_migration_fee_bps` of the pool, 0.2%,
  rounded down), as full-range liquidity at the migration price, rounded down. That is
  DBC's own `get_initial_liquidity_from_delta_quote`; the base side never binds, since
  the curve vault still holds the whole unsold supply. Tested: attacker migration, then
  1/1000 of the pool's liquidity added and locked by a third party, then graduation
  succeeds and pool fees are collected.
- **Which position is DBC's.** That liquidity check does not prove the position is the
  one DBC made. A third party can open a position, fund it with exactly the migrated
  liquidity, lock it for good and then hand its NFT account to our vault with Token-2022
  `SetAuthority(AccountOwner)`, which is how DBC hands over its own; it can do all this in
  the migration's own transaction, so even the fee checkpoints match. On chain the twin
  is indistinguishable: neither cp-amm's `Pool` and `Position` nor DBC's `VirtualPool`
  record which position the migration made, and the NFT mint is a free keypair. Were the
  twin recorded, `collect` would claim only its fees and the real position's would be
  stranded for good (costly griefing: about the whole graduation liquidity, locked).
  So `graduate` binds the position two ways. When it migrates by CPI, the recorded
  position must be the one cp-amm derives from the NFT mint that signed that call. When
  someone else migrated first, only the keeper may record it, naming the first position
  of DBC's migration transaction (the `first_position` account of the
  `migration_damm_v2` instruction in the transaction that created the pool). Tested
  (`a_twin_position_handed_to_the_vault_is_never_recorded`): an attacker migrates, then
  funds, locks and hands the vault a twin with exactly the expected liquidity; neither
  the twin nor even DBC's position can be recorded by anyone but the keeper, a twin its
  maker kept is refused even for the keeper (`NotLocked`), and the keeper records DBC's.
  Two coins at the same price and gradUsd lock the same liquidity; one's position is
  refused for the other (`WrongAccount`, tested). If someone migrated first, the keeper's
  migrating `graduate` fails on its accounts and is resent without the NFT mints.
- **Migration price** is fixed at config creation (`config.migration_sqrt_price`). The pool
  is seeded with `q*` from the constant threshold, not from the vault balance. Base comes
  from the vault balance, but the liquidity is `min(liquidity from base, liquidity from
  quote)` (`calculate_concentrated_initial_liquidity`). A donation to either vault
  therefore cannot move the price; donated base becomes leftover. Between curve completion
  and migration, all swaps are refused.
- **Side pools are possible and harmless to us.** A buyer can open coin/pToken or coin/USDC
  pools elsewhere (another DAMM v2 config, Raydium) at any price. They do not touch the
  migration. Aggregators may route through them, and arbitrage aligns prices after
  migration. The UI and indexer show only the canonical pool.
- **Look-alike configs**: see M8. Anyone can run DBC `create_config` with our pToken and
  `fee_claimer = our vault`. Only `launch` can sign for a config at
  `PDA(["dbc_config", mint], polypad_launch)`, and only `launch` writes `Launch` records.
- **Migration call** is permissionless (anyone can pay). The caller must sign with a fresh
  position NFT mint keypair, and with a second one when rounding leaves leftover liquidity
  (always pass the second set: DBC refuses if it is needed and missing). The payer covers
  the DAMM v2 rent, which DBC's pool authority advances (`flash_rent`). Measured: about
  145k CU (DBC's own tests budget 600,000). Our `graduate` makes this call by CPI (its
  payer and both NFT mints sign the outer transaction); no metadata or locker step is
  needed for our config (`migration_metadata` is deprecated in 0.2.1, and with no locked
  vesting the curve's completion moves the pool straight to `LockedVesting`).
- **LP lock**: DBC calls `permanent_lock_position` for the partner's 100%
  (`lock_liquidity_position`) and then hands the position NFT account to `fee_claimer`
  (`set_authority_for_position`). Permanently locked liquidity cannot be removed but keeps
  earning fees. If rounding leaves leftover liquidity, it goes to a *second* position
  owned by the DBC creator (the user), and with 0% creator lock it is **unlocked**. In the
  tests it was 0 at every price and gradUsd tried (0.1c and 99c, $1k, $6k and $1M).
  `graduate` requires the vault's position to hold exactly the migrated liquidity, all
  permanently locked (above).

## 5. Rust integration

**There is no CPI crate on crates.io** (`dynamic-bonding-curve`, `cp-amm`, `damm-v2` and
`dynamic-bonding-curve-sdk` all 404). Both repos pin `anchor-lang = "1.0.2"` (caret, so
it is semver-compatible with our 1.2.1), and the program crates have a `cpi` feature.
Depending on them by git, however, pulls `mpl-token-metadata 5.1.2-alpha.2`, `ruint`,
`spl-transfer-hook-interface`, `solana-instruction 3.x` and their vendored libs into our
lockfile.

**Built: `declare_program!`**. `solana/idls/dynamic_bonding_curve.json` (from the DBC SDK,
`src/idl/dynamic-bonding-curve/idl.json`, version 0.2.1) and `solana/idls/cp_amm.json`
(from the DAMM v2 SDK, `src/idl/cp_amm.json`, 0.2.5) are vendored, and
`declare_program!(dynamic_bonding_curve); declare_program!(cp_amm);` gives typed
`cpi::*` builders, the argument structs (no hand-written Borsh), `client::*` instruction
builders for tests and zero-copy account types. On anchor-lang 1.2.1 the zero-copy
accounts (`PoolConfig`, `VirtualPool`, `Pool`, `Position`) decode with
`AccountDeserialize` off chain; on chain the program reads only the fields it needs at
`offset_of!` offsets of the generated `repr(C)` types, which avoids copying ~1 KB
structs onto the 4 KB stack. Re-diff the IDLs on every Meteora release.

Instructions we use. Discriminators come from the IDL (`sha256("global:<name>")[..8]`),
and accounts are in IDL order (w = writable, s = signer).

| Instruction | Discriminator | Accounts |
|---|---|---|
| DBC `create_config(ConfigParameters)` | `c9cff3724b6f2fbd` | config (w,s: our PDA), fee_claimer, leftover_receiver, quote_mint, payer (w,s), system_program, event_authority, program |
| DBC `initialize_virtual_pool_with_spl_token({name, symbol, uri})` | `8c55d7b06636684f` | config, pool_authority `FhVo3m…`, creator (s), base_mint (w,s), quote_mint, pool (w), base_vault (w), quote_vault (w), mint_metadata (w), metadata_program `metaqb…`, payer (w,s), token_quote_program, token_program, system_program, event_authority, program |
| DBC `swap2({amount_0, amount_1, swap_mode})` | `414b3f4ceb5b5b88` | pool_authority, config, pool (w), input_token_account (w), output_token_account (w), base_vault (w), quote_vault (w), base_mint, quote_mint, payer (s), token_base_program, token_quote_program, referral_token_account (w, optional), event_authority, program. Remaining: the instructions sysvar, only for the first swap at the minimum fee. |
| DBC `swap({amount_in, minimum_amount_out})` | `f8c69e91e17587c8` | same as `swap2` (ExactIn only) |
| DBC `claim_trading_fee(max_a, max_b)` | `08ec5931987db151` | pool_authority, config, pool (w), token_a_account (w), token_b_account (w), base_vault (w), quote_vault (w), base_mint, quote_mint, fee_claimer (s), token_base_program, token_quote_program, event_authority, program |
| DBC `partner_withdraw_surplus` | `a8ad4864c962265c` | pool_authority, config, virtual_pool (w), token_quote_account (w), quote_vault (w), quote_mint, fee_claimer (s), token_quote_program, event_authority, program |
| DBC `withdraw_leftover` | `14c6caedebf3b742` | pool_authority, config, virtual_pool (w), token_base_account (w), base_vault (w), base_mint, leftover_receiver, token_base_program, event_authority, program |
| DBC `migration_damm_v2` | `9ca9e66735e45040` | virtual_pool (w), migration_metadata (deprecated; any), config, pool_authority (w), pool (w), first_position_nft_mint (w,s), first_position_nft_account (w), first_position (w), second_position_nft_mint (w,s, optional), second_position_nft_account (w, optional), second_position (w, optional), damm_pool_authority `HLnpSz…`, amm_program `cpamdp…`, base_mint (w), quote_mint (w), token_a_vault (w), token_b_vault (w), base_vault (w), quote_vault (w), payer (w,s), token_base_program, token_quote_program, token_2022_program, damm_event_authority, system_program. **Remaining[0]: DAMM v2 config `Hv8L…`** (not in the IDL account list). |
| cp-amm `claim_position_fee` | `b4269a118521a2d3` | pool_authority `HLnpSz…`, pool, position (w), token_a_account (w), token_b_account (w), token_a_vault (w), token_b_vault (w), token_a_mint, token_b_mint, position_nft_account, signer (s), token_a_program, token_b_program, event_authority, program |
| cp-amm `swap2(...)` | `414b3f4ceb5b5b88` | pool_authority, pool (w), input_token_account (w), output_token_account (w), token_a_vault (w), token_b_vault (w), token_a_mint, token_b_mint, payer (s), token_a_program, token_b_program, referral_token_account (w, optional), event_authority, program |

PDAs: DBC vaults `["token_vault", mint, pool]`. DAMM v2 vaults are the same seeds under
cp-amm. Position `["position", nft_mint]`, position NFT account
`["position_nft_account", nft_mint]` (a Token-2022 account), Metaplex metadata
`["metadata", metaqb…, mint]`. Event authority `["__event_authority"]` under each program.
In the DAMM v2 pool, token A is the coin and token B the pToken (DBC passes
`token_a_mint = base_mint`).

`swap_mode`: 0 ExactIn, 1 PartialFill, 2 ExactOut. `ConfigParameters` Borsh order is the
struct order in `process_create_config.rs`: `pool_fees, collect_fee_mode, migration_option,
activation_type, token_type, token_decimal, partner_liquidity_percentage,
partner_permanent_locked_liquidity_percentage, creator_liquidity_percentage,
creator_permanent_locked_liquidity_percentage, migration_quote_threshold,
sqrt_start_price, locked_vesting, migration_fee_option, token_supply,
creator_trading_fee_percentage, token_update_authority, migration_fee, migrated_pool_fee,
pool_creation_fee, partner_liquidity_vesting_info, creator_liquidity_vesting_info,
migrated_pool_base_fee_mode, migrated_pool_market_cap_fee_scheduler_params,
enable_first_swap_with_min_fee, compounding_fee_bps, padding[2], curve`.
`declare_program!` generates this, so it is not hand-encoded.

**Compute and transaction size**: measured, see [§8](#8-what-the-build-proved).

## 6. Anti-sniper

What DBC 0.2.1 offers (`dbc/base_fee/`, `dbc/CHANGELOG.md`):

| Tool | Status | Notes |
|---|---|---|
| Fee scheduler, linear or exponential | available | Time since `activation_point` (seconds or slots). The fee falls from `cliff_fee_numerator` (≤99%) to the value after `number_of_period` periods, which must be ≥0.25%. It applies to **buys and sells**, so early flips are taxed too. Fields: `first_factor = number_of_period`, `second_factor = period_frequency`, `third_factor = reduction_factor`. |
| Rate limiter (fee grows with buy size, single swap per tx) | **deprecated in 0.2.1**: new configs and pools are rejected | |
| Dynamic (volatility) fee | available | Capped at 20% of the base fee (`MAX_DYNAMIC_FEE_PERCENT`), so it is weak as an anti-sniper tool |
| `enable_first_swap_with_min_fee` | available | The pool's first swap pays the schedule's minimum fee, but **only if a top-level DBC pool-creation instruction for that pool comes earlier in the same transaction, the swap is itself a top-level DBC instruction, it passes the instructions sysvar as its remaining account, and it has no referral account** (`validate_contain_initialize_pool_ix_and_no_cpi` in `process_swap.rs`). A CPI'd pool creation never qualifies. |
| Delayed activation | not available | Pools activate at creation |

**Built (M5):** a linear scheduler from **25.4% to 1.4% over 60 s**:
`cliff_fee_numerator = 254_000_000`, `number_of_period = 60`, `period_frequency = 1`,
`reduction_factor = 4_000_000` (exact integers, minimum = 14,000,000; the start is the
coin's curve fee plus 24%). The tests measure a sniper's fee at 25.4% at second 0, 13.4%
at second 30 and 1.4% from second 60 on, to the atom, and the creator's first swap at
1.4%. Cost: Meteora takes 20% of the high early fees; `launch` cannot read the pool back
(it is created after it), so the record stores the pool's address and the quote's expiry,
and the fee vault checks every account against the first and the pool's creation time
against the second (§4).

## 7. Risks and gotchas

1. **Meteora trust.** Both programs are upgradeable (mainnet authority
   `JADaUV8kvDpDbJr55wxXJHVaBS3VCj8thZZHjfeuCVLd`). DAMM v2 operators can **pause a pool**
   (`set_pool_status`), **raise its base fee up to 10%** (`update_pool_fees`,
   `MAX_FEE_NUMERATOR_POST_UPDATE`) **and change or replace its dynamic-fee parameters**
   in the same instruction (`DynamicFeeUpdateMode::Update`, `cp-amm/state/pool.rs`), so
   the total fee is not capped at 10%. Document this next to "Circle can freeze USDC".
   The assertions after our CPIs (§2) catch math drift on the curve side.
2. **pToken goes to $0 (market resolves NO).** The exchange refuses mints at $0 (v14 rule),
   so nobody can get new pTokens. Existing holders can still buy coins with worthless
   pTokens, and every sale pays out worthless pTokens. The coin's dollar price is 0 on the
   curve and in the pool. Accrued fees are worthless. Nothing breaks on chain: an
   unfinished curve simply never graduates (DBC has no expiry), and a graduated pool keeps
   its locked LP. `launch` refuses paused and settled markets. Product work: show
   "market resolved" on the coin, stop routing buys, stop the migration keeper for that
   market, and stop counting its fees. At $1 (YES), a coin sale gives pTokens that redeem
   at $1.
3. **No auto-migration for pToken quotes** (M6). If our keeper is down, graduated coins
   freeze. Mitigations: alert on `EvtCurveComplete` without migration within N seconds; the
   manual migrator; the instruction is permissionless. Devnet has no Meteora keepers at
   all.
4. **Quote-mint rules.** A classic SPL pToken is fine with any authorities. DBC does not
   check freeze authority, and DAMM v2 migration skips mint validation
   (`CreatePoolWithoutMintValidation`). If pToken ever moved to Token-2022 with any
   extension beyond metadata, DBC would need a Meteora operator **token badge**, and
   transfer fees must be zero forever (`QuoteMintHasNonZeroTransferFee`). Keep pToken classic.
5. **Metadata limits** 32/10/200 bytes (§2). The metadata is immutable with our settings;
   update authority goes to the system program.
6. **Rent per launch**, paid by the user: about 0.027 SOL (DBC config ≈0.0082, pool
   ≈0.0038, two vaults ≈0.0041, mint ≈0.0015, metadata ≈0.0056, our `Launch` record
   ≈0.0039), plus a new market's pToken accounts on its first coin. **A DBC config and
   virtual pool cannot be closed**, so this rent is permanent. The first `collect` creates
   the vault's coin ATA (≈0.002 SOL, paid by the caller), and a market's pToken ATA and
   `Owed` accounts once. Migration costs the migrator about 0.02–0.03 SOL in DAMM v2
   accounts.
7. **Trading is frozen between curve completion and migration** (§2). The UI should show
   "graduating".
8. **Fees net of Meteora**: the creator gets 0.56% on the curve and 0.4–0.48% on the
   pool, not 0.7%/0.5% (§3, open decision).
9. **Terminals and Jupiter**: DBC and DAMM v2 are indexed, but a coin/pToken pair has no
   USD price unless pToken itself is priced (decision D5). DexScreener will show prices in
   pToken.
10. **Look-alike coins and extra pools on our configs** (M8, §4): the indexer recognises our
    coins by `Launch` record and its `pool`, opened by `pool_deadline`. Their partner fees
    are swept to the platform (`sweep_fees`), and once anyone migrates one of them (or a
    late coin's pool), the fees of the position our vault receives
    (`sweep_position_fees`); the keeper should sweep pools with meaningful volume now and
    then. A twin position someone handed our vault in a real coin's pool (§4) is refused
    by both and its fees stay in DAMM v2: it was the attacker's money.
11. **Unlocked dust LP** to the creator at migration (§4); 0 in the tests.
12. **Holder rewards** stay ours (D3). DBC and DAMM v2 know nothing about them.
13. **Devnet is not mainnet** (§1): different DBC and cp-amm builds.
14. **A second flat-1% DBC-only DAMM v2 config** (§4). Today `Hv8L…` is the only one, so
    DBC lets nobody migrate our coins anywhere else. If Meteora adds one, a front-run
    migration into it would leave that coin unrecorded by `graduate` and its pool fees
    unclaimed until a program upgrade. `fetch.sh` warns when the set changes; the
    keeper's `graduate` right at completion leaves a front-runner one slot.
15. **Recording a migration someone else ran trusts the keeper** (§4). A keeper that named
    the wrong vault-owned locked position would strand the real one's pool fees (it can
    move no funds). The keeper takes the first position from DBC's migration transaction;
    a stolen keeper key can do this harm only to coins someone else migrated first.
    Built (`findMigrationPosition`, lib/solana/keeper.ts): the keeper reads the DBC
    curve's newest signatures (a migrated curve takes no more swaps, so its migration is
    among its last transactions) and walks back to the successful transaction running
    `migration_damm_v2` from that curve to that pool (top level or by CPI), and names its
    `first_position` (account 7) and `first_position_nft_account` (6). Not the DAMM pool's
    oldest signatures: that address is known before the pool exists, so anyone could fill
    its early history with ~200 cheap transactions that only name it and hide the
    migration. A search reads at most 3 pages of signatures and 300 transactions; a failed
    one backs off per coin (10 s doubling to hourly, `keeper:sol:record-failed`) and shows
    in the `keeper_sol_record_stuck_coins` gauge, so a stuck coin pages someone. It records
    nothing while that transaction cannot be read, never "the position whose NFT the vault
    holds". Checked against the recorded
    mainnet-binary migration (`services/solana/core.test.ts`) and against a twin handed to
    the vault (`services/keeper/solana.test.ts`).

## Instruction flow

| Step | Transactions (built by our API) | Notes |
|---|---|---|
| Launch, no first buy | one v0 transaction: `ed25519(quote)`, **`polypad_launch::launch`** (verifies the quote, CPI `polypad_exchange::create_market` as the factory signer, CPI DBC `create_config` signed by the config PDA, reads the config back, writes the `Launch` PDA), DBC `initialize_virtual_pool_with_spl_token` (creator and mint sign) | Fits in 1,232 bytes when name + symbol + uri are short (≤ ~150 bytes together); otherwise send it as the bundle below |
| Launch with the creator's first buy | **Jito bundle of two**: tx A `ed25519(quote)`, `launch` with `first_buy` (adds a CPI `polypad_exchange::mint` of the creator's pTokens at the same quote); tx B DBC `initialize_virtual_pool_with_spl_token`, ATA create (creator's coin account), DBC `swap2` PartialFill with the instructions sysvar (first swap, curve fee) | One transaction would be 1,337 bytes or more (§8). The mint signs both. tx B must land before the quote's `valid_until` (the bundle lands both in one slot); if it is dropped the creator may resend it until then, and nobody else can create that pool. After that the record expires (`expire_launch`, anyone). Base mint can be ground to a vanity suffix client-side |
| Buy on the curve | `ed25519(quote)`, `polypad_exchange::mint` (USDC → pToken), DBC `swap2` PartialFill | Pass our referral account in production |
| Sell on the curve | DBC `swap2` ExactOut (coin → exact pToken), `ed25519(quote)`, `polypad_exchange::redeem` | |
| Graduation | keeper: **`polypad_launch::graduate`** alone (CPIs DBC `migration_damm_v2` with `Hv8L…`, then records the pool and checks the vault's locked liquidity), one v0 transaction signed by the keeper and two fresh position NFT mints | Triggered by `EvtCurveComplete`. If someone migrated first, the keeper resends `graduate` without the NFT mints, naming the first position of DBC's migration transaction (only the keeper may). Then DBC `withdraw_leftover` and `polypad_launch::burn_leftover` |
| Trade after graduation | `ed25519`, exchange `mint`/`redeem`, cp-amm `swap2` with our referral | |
| Fees | **`polypad_launch::collect`** (anyone) → DBC `claim_trading_fee` + cp-amm `claim_position_fee` → split per coin; `withdraw` (payee, platform), `withdraw_holders` (keeper → payout wallet), `propose_payee`/`accept_payee`; `sweep_fees` and `sweep_position_fees` (keeper) for pools that are not coins and their migrated positions | |
| Expired launch | `polypad_launch::expire_launch` (anyone) once the quote has expired with no pool, or with a pool opened late | Rent back to the creator; the coin is never shown. Its pool's partner fees and, if anyone migrates it, its position's fees go to the platform (`sweep_fees`, `sweep_position_fees`) |

## 8. What the build proved

`solana/programs/polypad_launch` (Anchor 1.2.1, ~750 KB), tested in LiteSVM against the
mainnet DBC (0.2.1), cp-amm (0.2.4) and Metaplex binaries and the real `Hv8L…`, 2% and
`A8gM…` config accounts (`tests/fixtures/fetch.sh` dumps them, ~5 MB of binaries, not
committed; the config JSONs are). Program id `GDe9PRUW6Tq2McNSLTfXMETk4PgjHhyCYm3JvneKxLzn`.

| Transaction | Compute units | Bytes (v0 + one lookup table of 24 shared accounts) | Accounts | Fits one transaction? |
|---|---|---|---|---|
| Launch, no first buy (`launch` ~140k, DBC pool creation ~96–103k) | ~228–244k | 1,121 (31 B uri) · 1,146 (32 B name, 31 B uri) · 1,183 (67 B uri) · 1,316 (200 B uri) | 27 | Yes up to ~150 bytes of strings; with the lookup table only |
| Launch + first buy in one transaction | ~345–380k | 1,337–1,526 | 34 | **No**, even with the lookup table: hence the bundle |
| Bundle tx A: `launch` with the first-buy mint | ~180–208k | 973 | 27 | Yes |
| Bundle tx B: pool creation + ATA + first `swap2` | ~144–152k | 772–967 (200 B uri) | 19 | Yes |
| Buy: ed25519 + exchange `mint` + DBC `swap2` | ~74k (mint 44k, swap 30k) | 1,030 | 25 | Yes |
| Sell: DBC `swap2` + ed25519 + exchange `redeem` | ~67k | 1,129 | 28 | Yes |
| Buy after graduation: ed25519 + `mint` + DAMM v2 `swap2` | ~51k | 997 | 24 | Yes |
| Graduation: `graduate` with DBC `migration_damm_v2` by CPI | ~177–197k (DBC ~145k) | 933 | 30 | Yes with the lookup table |
| `graduate` recording a migration someone else ran (keeper) | ~24–26k | 677 | 26 | Yes; DBC's migration alone fits as a legacy transaction (1,107 bytes, ~142k) |
| `collect`, curve only / curve + pool | ~107k / ~95k | 644 / 807 | 23 / 31 | Yes |
| `sweep_fees` (keeper, one look-alike pool) | ~81–90k | 669 | 24 | Yes |
| `sweep_position_fees` (keeper, a migrated late pool) | ~77k | 702 | 25 | Yes |

Compute varies by a few thousand units with PDA bump searches. The lookup table holds the
programs, sysvars, the exchange config, float, USDC mint and event authority, our factory,
fee vault and event authority, both pools' authorities and event authorities, and `Hv8L…`.

Checklist from the research, now verified against the mainnet programs:

- [x] Launch at 60c with the exact parameters above; `PoolConfig` equals the table.
- [x] Buy to completion with PartialFill: the final fill stops at the migration price
      (`quote_reserve = q* + 1`), the unused pTokens stay with the buyer, every swap is
      refused until migration.
- [x] `migration_damm_v2` with `Hv8L…`: the pool opens at the curve's last price with
      9,980 shares, the position is permanently locked and owned by the vault PDA, supply
      drops to `post` (918,367,353,726,637), the second position held 0 liquidity, 3 atoms
      of leftover burned.
- [x] Launch → complete → `graduate` → pool buy and sell → `collect` at 0.1c and 99c with
      gradUsd $1k, $6k and $1M: DBC's config equals ours, the vault's locked liquidity
      equals `migrated_liquidity` and the pool's whole liquidity, the vault never receives
      coins.
- [x] Graduation against third parties: an attacker migrates first and then adds and
      locks liquidity, and opens an unfunded position for our vault; `graduate` still
      records the vault's real position and pool fees are collected. Migration through the
      2% config or `A8gM…` is refused by DBC, and `graduate` refuses any other config or
      pool address.
- [x] A pool opened after the launch quote expired: `collect` and `graduate` refuse it,
      `expire_launch` closes the record (rent to the creator), `sweep_fees` then takes its
      partner fees for the platform; migrated by a stranger, its locked position's fees go
      to the platform through `sweep_position_fees`.
- [x] A twin position (funded with exactly the migrated liquidity, locked, NFT handed to
      the vault) is never recorded: only the keeper records a migration it did not run;
      another coin's position with the same liquidity, and a twin its maker kept, are
      refused even for the keeper. `expire_launch` refuses any pool account but the
      record's, and `burn_leftover` burns only after graduation.
- [x] `claim_trading_fee` and `claim_position_fee` through CPI from our PDA; fees in
      pToken only; split and withdrawn.
- [x] CU and transaction size of launch + first buy with a lookup table: bundle.
- [x] `declare_program!` on anchor-lang 1.2.1, including zero-copy accounts.
- [x] Look-alike config and a second pool on our config: refused by `collect` and absent
      from our records; their partner fees swept to the platform by `sweep_fees`, which
      refuses a coin's own pool and a look-alike quoted in a coin.
- [x] `check_config` catches a change to any field it compares (each flipped in a test);
      the platform may equal a payee and both sides are credited and withdrawn apart.
- [ ] Repeat on a Surfpool mainnet fork with real USDC, Squads and a real Jito bundle;
      then devnet end to end once the services exist (Phase 3).
