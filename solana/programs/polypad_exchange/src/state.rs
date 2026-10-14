//! Accounts. One `Config` holds what `PriceOracle` and `PExchange` kept as
//! contract-wide state; one `Market` per position id holds both contracts'
//! per-market mappings. Seeds use distinct literal prefixes and fixed-length
//! parts only, so no two kinds of account can ever derive the same address.

use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"exchange";
pub const VAULT_SEED: &[u8] = b"float";
pub const MARKET_SEED: &[u8] = b"market";
pub const PTOKEN_SEED: &[u8] = b"ptoken";
pub const HELD_SEED: &[u8] = b"held";
pub const FILL_SEED: &[u8] = b"fill";
pub const CLAIM_SEED: &[u8] = b"claim";
pub const ASIDE_SEED: &[u8] = b"aside";
pub const DELAYED_SEED: &[u8] = b"delayed";

/// $1, and one share: prices, USDC and pTokens all use six decimals.
pub const ONE: u64 = 1_000_000;
pub const BPS: u64 = 10_000;
pub const PTOKEN_DECIMALS: u8 = 6;

/// Delay before a new signer takes effect. Revoking is immediate.
pub const ROLE_DELAY: i64 = 12 * 3_600;
/// A proposed signer must be accepted within this long after `ROLE_DELAY`.
pub const ACCEPT_WINDOW: i64 = 7 * 86_400;
/// A settlement takes effect this long after it is recorded; the owner can
/// cancel it meanwhile, so one stolen keeper key cannot settle a cheap market
/// at $1 and redeem the float out of it.
pub const SETTLE_DELAY: i64 = 3_600;
/// Delay before a changed bridge deposit takes effect.
pub const BRIDGE_DELAY: i64 = 2 * 86_400;
/// How long the part of a sale over the hourly cap waits before `release`.
pub const DELAY: i64 = 3_600;
/// The smallest part of a sale that is delayed ($1).
pub const MIN_DELAYED: u64 = 1_000_000;
pub const HOUR: i64 = 3_600;
/// Queue claims every mint and payment tries to pay first, as on the EVM.
pub const QUEUE_BATCH: usize = 3;
/// Most claims one `pay_queue` call pays: keeps it inside the compute budget.
pub const MAX_PAY_QUEUE: usize = 10;
/// Upper bounds `set_params` and `set_settle_fee` allow.
pub const MAX_SPREAD_BPS: u16 = 500;
pub const MAX_SETTLE_FEE_BPS: u16 = 200;
pub const MIN_BAND_PRICE: u64 = 1_000;
pub const MAX_LABEL_NAME: usize = 64;
pub const MAX_LABEL_SYMBOL: usize = 16;

/// The free float at the first mint or redeem of `hour`, before that trade
/// moved it (`amount`), and the hour's cap base: the lower of it and the last
/// such snapshot (`base`).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct HourFloat {
    pub hour: u64,
    pub amount: u64,
    pub base: u64,
}

/// Metered USDC per clock hour. The EVM mapping is only ever read for the
/// current hour and the one before, so two slots hold all of it.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct Outflow {
    pub cur_hour: u64,
    pub cur: u64,
    pub prev_hour: u64,
    pub prev: u64,
}

impl Outflow {
    pub fn at(&self, hour: u64) -> u64 {
        if self.cur_hour == hour {
            self.cur
        } else if self.prev_hour == hour {
            self.prev
        } else {
            0
        }
    }

    /// Add to `hour`, rolling the slots forward when the hour has turned. An
    /// hour earlier than the current slot (a clock that stepped back) adds to
    /// the current slot, so nothing metered is ever dropped.
    pub fn add(&mut self, hour: u64, amount: u64) -> Result<()> {
        if hour > self.cur_hour {
            if self.cur_hour.checked_add(1) == Some(hour) {
                (self.prev_hour, self.prev) = (self.cur_hour, self.cur);
            } else {
                (self.prev_hour, self.prev) = (0, 0);
            }
            (self.cur_hour, self.cur) = (hour, 0);
        }
        self.cur = self.cur.checked_add(amount).ok_or(crate::error::ExchangeError::MathOverflow)?;
        Ok(())
    }
}

/// `PriceOracle` + `PExchange` contract-wide state. PDA `["exchange"]`; it is
/// also the authority of the float vault and every pToken mint.
#[account]
#[derive(InitSpace, Debug)]
pub struct Config {
    pub owner: Pubkey,
    /// Two-step ownership: the new owner accepts.
    pub pending_owner: Pubkey,
    /// Pauses and settles markets, reports backing, sends float to the bridge, may halt.
    pub keeper: Pubkey,
    /// May create pTokens for new markets (the launch program's signer).
    pub factory: Pubkey,
    /// Signs quotes (ed25519). Default = revoked: no quote verifies.
    pub signer: Pubkey,
    pub pending_signer: Pubkey,
    pub pending_signer_at: i64,
    /// Pinned at initialize: the only mint the float is held in.
    pub usdc_mint: Pubkey,
    pub vault: Pubkey,
    /// Polymarket bridge deposit token account. Default = none.
    pub bridge_deposit: Pubkey,
    pub pending_bridge_deposit: Pubkey,
    pub pending_bridge_deposit_at: i64,
    /// Set once a bridge deposit has been in place; later ones wait BRIDGE_DELAY.
    pub bridge_ever_set: bool,
    /// Every mint, redeem, release and queue payment refused while set.
    pub halted: bool,
    pub buy_spread_bps: u16,
    pub sell_spread_bps: u16,
    /// Fee on redemptions of a settled market, bps of the payout.
    pub settle_fee_bps: u16,
    pub min_price: u64,
    pub max_price: u64,
    /// Whether sales at a quote count against the hourly cap too. Payouts of
    /// settled markets always do.
    pub meter_sales: bool,
    /// USDC a market's unbacked supply may lose if its price went to $1, unless overridden.
    pub default_max_risk: u64,
    /// USDC all markets' unbacked supply together may lose.
    pub max_total_risk: u64,
    /// Sum of every market's stored `risk`, 12 decimals.
    pub total_risk: u128,
    /// The hourly redemption cap is never below this, USDC.
    pub outflow_floor: u64,
    /// The hourly redemption cap as a share of the float, bps.
    pub outflow_float_bps: u64,
    pub hour_float: HourFloat,
    pub outflow: Outflow,
    /// USDC owed to the queue, including claims set aside for their owners.
    pub queued: u64,
    /// Set-aside claims' USDC: reserved for those owners, never paid to the queue.
    pub unclaimed_total: u64,
    /// The queue is `claim_head..claim_tail`, oldest first.
    pub claim_head: u64,
    pub claim_tail: u64,
    /// Next index for a claim set aside at a release (its own namespace, never in line).
    pub aside_next: u64,
    /// Next delayed-sale ticket.
    pub delayed_next: u64,
    pub bump: u8,
    pub vault_bump: u8,
}

impl Config {
    pub fn is_owner(&self, key: &Pubkey) -> bool {
        self.owner == *key
    }

    pub fn is_keeper(&self, key: &Pubkey) -> bool {
        self.keeper != Pubkey::default() && self.keeper == *key
    }
}

/// One market: the oracle's `Status` and the exchange's per-market mappings.
/// PDA `["market", position_id]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Market {
    /// The CTF position id (also the CLOB token id), big-endian: the EVM uint256.
    pub position_id: [u8; 32],
    /// pToken mint, PDA `["ptoken", position_id]`.
    pub mint: Pubkey,
    /// The exchange's own pToken account, PDA `["held", position_id]`: shares
    /// held for delayed sales, and fee shares sent here to be absorbed (as
    /// may any other pToken account the exchange PDA owns, its associated
    /// token account included).
    pub held: Pubkey,

    pub paused: bool,
    /// Whether it was paused before a settlement was recorded, to restore on cancel.
    pub paused_before_settle: bool,
    pub payout: u64,
    /// When the payout takes effect; 0 = not settled.
    pub settle_at: i64,
    /// Who recorded the settlement now pending or in effect.
    pub settled_by: Pubkey,
    /// A keeper whose settlement here the owner cancelled: it may not record another.
    pub barred: Pubkey,

    /// Shares the desk holds on Polymarket, as last reported, less burns since.
    pub backed: u64,
    /// 0 = use `default_max_risk`.
    pub max_risk_override: u64,
    /// What the unbacked supply could cost to cover at $1: shares x (1 - price), 12 decimals.
    pub risk: u128,
    /// Lowest price minted at since the market was last fully backed (0 = unknown, taken as $0).
    pub low: u64,
    /// Highest price minted at since then (ONE = unknown, taken as $1).
    pub high: u64,
    /// Shares minted since the keeper last reported the market.
    pub minted_since_report: u64,
    /// Every share the exchange has ever minted in this market.
    pub minted_total: u64,
    /// Every share ever burned in this market. Holders can burn their own
    /// pTokens straight through the token program, which the exchange never
    /// sees; each instruction that reads the supply first counts those, so
    /// afterwards `burned_total == minted_total - supply` (`Config::burned_outside`).
    pub burned_total: u64,
    /// The lowest supply seen since the last backing report (the supply
    /// then, lowered by every burn; a direct burn shows at the next
    /// instruction, before any mint can lift the supply again). The desk
    /// sells only shares beyond the supply, so whatever it read since that
    /// report it still holds at least down to this (`Config::report`).
    pub low_supply: u64,
    /// `burned_total` at the last backing report: how low the supply could
    /// have gone between a read taken before that report and the report.
    pub burned_at_report: u64,
    /// pToken held in `held` for live delayed sales: `absorb` leaves it alone.
    pub delayed_shares: u64,

    #[max_len(64)]
    pub name: String,
    #[max_len(16)]
    pub symbol: String,

    pub bump: u8,
    pub mint_bump: u8,
    pub held_bump: u8,
}

impl Market {
    /// Whether a recorded payout has taken effect.
    pub fn settled(&self, now: i64) -> bool {
        self.settle_at != 0 && now >= self.settle_at
    }
}

/// How much of one signed quote has been used (the EVM `quoteFilled[digest]`).
/// PDA `["fill", sha256(message)]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct QuoteFill {
    pub filled: u64,
    pub valid_until: i64,
    /// Who paid the rent; `close_fill` refunds them.
    pub payer: Pubkey,
    pub bump: u8,
}

/// USDC owed to `to` (a USDC token account). In line at `["claim", index]`
/// until paid; a claim whose recipient cannot be paid is set aside for its
/// owner (`set_aside`), as are payments a release could not deliver
/// (`["aside", index]`). Set-aside claims are paid only to `to`, by
/// `withdraw_unclaimed`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Claim {
    pub index: u64,
    pub to: Pubkey,
    pub amount: u64,
    pub payer: Pubkey,
    pub set_aside: bool,
    pub bump: u8,
}

/// The part of a sale over the hourly cap: `p_amount` pToken held in the
/// market's `held` account, owed `amount` USDC once released. PDA
/// `["delayed", ticket]`, closed when released or cancelled.
#[account]
#[derive(InitSpace, Debug)]
pub struct DelayedSale {
    pub ticket: u64,
    /// USDC token account the sale pays.
    pub to: Pubkey,
    /// That account's owner at the sale: a cancel returns the pToken to them.
    pub to_owner: Pubkey,
    pub ready_at: i64,
    pub position_id: [u8; 32],
    pub p_amount: u64,
    pub amount: u64,
    pub payer: Pubkey,
    pub bump: u8,
}
