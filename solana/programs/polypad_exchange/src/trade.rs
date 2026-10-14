//! Mint, redeem, release, cancel and absorb: `PExchange`'s `_mint`,
//! `_mintSettled`, `_redeem`, `release`, `cancel` and `absorb`, in the same
//! order of checks and effects. Token CPIs come after the bookkeeping they
//! depend on, except where a fresh supply or balance must be read after them.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, TransferChecked};

use crate::error::ExchangeError;
use crate::events::{Absorbed, Cancelled, Minted, Redeemed, Released, SaleDelayed};
use crate::math::{hour_of, mint_out, redeem_out};
use crate::quote::{self, Quote, BUY};
use crate::state::*;
use crate::treasury::{create_pda, mint_supply, token_amount, write_account, Float, Shares, Slots};

/// What a mint is priced at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// A verified quote: the band and the unbacked-risk cap apply.
    Quote,
    /// A payout: settled (`final_`), or a $1 payout still inside its delay.
    /// The USDC stays in the float as the backing, so no band and no cap.
    Payout { final_: bool },
}

/// Whether a mint is at a payout instead of a quote (`_payoutMint`): settled,
/// or a $1 payout recorded and still inside its delay. A share sold at $1 can
/// cost the float nothing whatever the outcome, so buying never waits.
pub fn payout_mint(m: &Market, now: i64) -> Option<At> {
    if m.settled(now) {
        return Some(At::Payout { final_: true });
    }
    if m.settle_at != 0 && m.payout == ONE {
        return Some(At::Payout { final_: false });
    }
    None
}

/// The price in a quote after `PriceOracle._verify` and `PExchange._quoted`:
/// signed by the configured signer over exactly these fields, live, for this
/// side and market, the market open for it, and the quote's total size not
/// passed across every transaction that used it.
#[allow(clippy::too_many_arguments)]
pub fn quoted(
    config: &Config,
    m: &Market,
    fill: &mut QuoteFill,
    instructions: &AccountInfo,
    q: &Quote,
    side: u8,
    amount: u64,
    now: i64,
) -> Result<u64> {
    require!(q.position_id == m.position_id, ExchangeError::QuoteForOtherMarket);
    require!(config.signer != Pubkey::default(), ExchangeError::NoSigner);
    let price = quote::verify(instructions, q, side, &config.signer, &crate::ID, now)?;
    require!(!m.settled(now), ExchangeError::MarketSettled);
    require!(side != BUY || !m.paused, ExchangeError::MarketPaused);
    // A quote covers `max_amount` in total, however many transactions use it:
    // replaying one (it binds no taker) cannot multiply its size.
    let used = fill.filled.checked_add(amount).ok_or(ExchangeError::MathOverflow)?;
    require!(used <= q.max_amount, ExchangeError::QuoteTooSmall);
    fill.filled = used;
    Ok(price)
}

/// Record who paid a fresh fill's rent and when it may be closed. Done before
/// any branch, so a fill created by a mint at a payout is closable too.
pub fn open_fill(fill: &mut QuoteFill, bump: u8, payer: Pubkey, q: &Quote) {
    if fill.payer == Pubkey::default() {
        fill.payer = payer;
        fill.valid_until = q.valid_until;
        fill.bump = bump;
    }
}

pub struct Buyer<'info> {
    pub buyer: AccountInfo<'info>,
    pub buyer_usdc: AccountInfo<'info>,
    pub to: AccountInfo<'info>,
}

#[allow(clippy::too_many_arguments)]
pub fn buy<'info>(
    f: &mut Float<'_, 'info>,
    m: &mut Market,
    shares: &Shares<'info>,
    who: &Buyer<'info>,
    usdc_in: u64,
    min_out: u64,
    price: u64,
    at: At,
    now: i64,
    queue: &'info [AccountInfo<'info>],
) -> Result<()> {
    require!(!f.config.halted, ExchangeError::Halted);
    if let At::Payout { .. } = at {
        require!(price != 0, ExchangeError::SettledAtZero);
    }
    let vault = f.vault_amount()?;
    f.config.mark_hour(now, vault);
    if at == At::Quote {
        require!(price >= f.config.min_price && price <= f.config.max_price, ExchangeError::PriceOutOfBand);
    }
    let out = mint_out(price, usdc_in, f.config.buy_spread_bps)?;
    require!(out > 0, ExchangeError::ZeroAmount);
    require!(out >= min_out, ExchangeError::Slippage);
    // Count any direct burns first, so the risk check sees the real supply
    // and backing that is within it.
    let supply = mint_supply(&shares.mint)?;
    f.config.burned_outside(m, supply);
    if at == At::Quote {
        f.config.check_risk(m, supply, out, price)?;
    }
    m.minted_total = m.minted_total.checked_add(out).ok_or(ExchangeError::MathOverflow)?;

    token::transfer_checked(
        CpiContext::new(
            f.token_program.key(),
            TransferChecked {
                from: who.buyer_usdc.clone(),
                mint: f.usdc_mint.clone(),
                to: f.vault.clone(),
                authority: who.buyer.clone(),
            },
        ),
        usdc_in,
        f.usdc_decimals,
    )?;
    shares.mint_to(&who.to, out)?;
    // A pending $1 payout keeps the stored risk in case it is cancelled.
    if at == (At::Payout { final_: true }) {
        f.config.store_risk(m, 0, 0, 0)?;
    }
    f.emit(&Minted { position_id: m.position_id, to: *who.to.key, usdc_in, p_out: out, price })?;
    if f.config.queued > 0 {
        f.pay_queue(QUEUE_BATCH, queue, true)?;
    }
    Ok(())
}

pub struct Seller<'a, 'info> {
    pub seller: AccountInfo<'info>,
    pub seller_ptoken: AccountInfo<'info>,
    pub held: AccountInfo<'info>,
    /// USDC token account paid.
    pub to: AccountInfo<'info>,
    pub to_owner: Pubkey,
    pub new_ticket: Option<&'a AccountInfo<'info>>,
}

#[allow(clippy::too_many_arguments)]
pub fn sell<'info>(
    f: &mut Float<'_, 'info>,
    m: &mut Market,
    shares: &Shares<'info>,
    who: &Seller<'_, 'info>,
    slots: &mut Slots<'_, 'info>,
    amount_in: u64,
    min_out: u64,
    price: u64,
    settled: bool,
    now: i64,
    queue: &'info [AccountInfo<'info>],
) -> Result<()> {
    require!(!f.config.halted, ExchangeError::Halted);
    let vault = f.vault_amount()?;
    f.config.mark_hour(now, vault);
    let fee = if settled { f.config.settle_fee_bps } else { f.config.sell_spread_bps };
    let out = redeem_out(price, amount_in, fee)?;
    require!(out >= min_out, ExchangeError::Slippage);
    f.emit(&Redeemed { position_id: m.position_id, to: *who.to.key, p_in: amount_in, usdc_out: out, price })?;
    if out == 0 {
        burn_from_seller(f, shares, who, amount_in)?;
        let supply = mint_supply(&shares.mint)?;
        return f.config.burned(m, amount_in, supply, now);
    }

    // Payouts of settled markets are always metered: a settlement recorded
    // by a stolen keeper key and missed by the owner would otherwise pay the
    // whole float in one transaction; metered, the part over the hour's
    // budget waits as a ticket the owner can still cancel. Sales at a quote
    // are metered only while `meter_sales` is on.
    let metered = settled || f.config.meter_sales;
    let remaining = if metered { f.config.outflow_remaining(now, vault) } else { out };
    let mut now_ = out.min(remaining);
    // Dust tickets would only flood the release scan and the alert: less than
    // MIN_DELAYED over the cap is paid now if the cap was not yet spent (so it
    // is passed by under $1 an hour), and refused if it was.
    if out - now_ < MIN_DELAYED {
        require!(now_ != 0, ExchangeError::DustOverCap);
        now_ = out;
    }
    if metered {
        f.config.outflow.add(hour_of(now), now_)?;
    }
    // The part over the cap waits: its pToken is held here, its price fixed.
    let p_later = if now_ < out { (amount_in as u128 * (out - now_) as u128 / out as u128) as u64 } else { 0 };
    if p_later > 0 {
        token::transfer_checked(
            CpiContext::new(
                shares.token_program.key(),
                TransferChecked {
                    from: who.seller_ptoken.clone(),
                    mint: shares.mint.clone(),
                    to: who.held.clone(),
                    authority: who.seller.clone(),
                },
            ),
            p_later,
            PTOKEN_DECIMALS,
        )?;
    }
    burn_from_seller(f, shares, who, amount_in - p_later)?;
    if p_later > 0 {
        m.delayed_shares = m.delayed_shares.checked_add(p_later).ok_or(ExchangeError::MathOverflow)?;
        let ticket = f.config.delayed_next;
        let slot = who.new_ticket.ok_or(ExchangeError::MissingAccount)?;
        let idx = ticket.to_le_bytes();
        let bump = create_pda(&slots.payer, slot, &slots.system_program, &[DELAYED_SEED, &idx], 8 + DelayedSale::INIT_SPACE)?;
        let ready_at = now.checked_add(DELAY).ok_or(ExchangeError::MathOverflow)?;
        let amount = out - now_;
        write_account(
            slot,
            &DelayedSale {
                ticket,
                to: *who.to.key,
                to_owner: who.to_owner,
                ready_at,
                position_id: m.position_id,
                p_amount: p_later,
                amount,
                payer: *slots.payer.key,
                bump,
            },
        )?;
        f.config.delayed_next += 1;
        f.emit(&SaleDelayed { ticket, position_id: m.position_id, to: *who.to.key, p_amount: p_later, amount, ready_at })?;
    }
    // Shares held for the delayed part still count: account once they are back.
    let supply = mint_supply(&shares.mint)?;
    f.config.burned(m, amount_in - p_later, supply, now)?;
    if now_ > 0 {
        f.pay(&who.to, now_, false, slots, queue)?;
    }
    Ok(())
}

fn burn_from_seller<'info>(f: &Float<'_, 'info>, shares: &Shares<'info>, who: &Seller<'_, 'info>, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    token::burn(
        CpiContext::new(
            f.token_program.key(),
            Burn { mint: shares.mint.clone(), from: who.seller_ptoken.clone(), authority: who.seller.clone() },
        ),
        amount,
    )
}

/// Pay a delayed sale once `DELAY` has passed: its pToken is burned and its
/// USDC paid, queued if the float is short, or set aside if its recipient
/// cannot be paid. Never metered: only the part paid at the sale was.
#[allow(clippy::too_many_arguments)]
pub fn release<'info>(
    f: &mut Float<'_, 'info>,
    m: &mut Market,
    shares: &Shares<'info>,
    held: &AccountInfo<'info>,
    t: &DelayedSale,
    to: &AccountInfo<'info>,
    slots: &mut Slots<'_, 'info>,
    now: i64,
    queue: &'info [AccountInfo<'info>],
) -> Result<()> {
    require!(!f.config.halted, ExchangeError::Halted);
    require!(now >= t.ready_at, ExchangeError::NotYet);
    m.delayed_shares = m.delayed_shares.checked_sub(t.p_amount).ok_or(ExchangeError::MathOverflow)?;
    shares.burn_held(held, t.p_amount)?;
    let supply = mint_supply(&shares.mint)?;
    f.config.burned(m, t.p_amount, supply, now)?;
    f.emit(&Released { ticket: t.ticket, position_id: t.position_id })?;
    f.pay(to, t.amount, true, slots, queue)
}

/// Undo a delayed sale before it is released: the pToken goes back to the
/// sale's payee and the USDC is never paid. Works while halted.
pub fn cancel<'info>(
    m: &mut Market,
    shares: &Shares<'info>,
    held: &AccountInfo<'info>,
    dest: &AccountInfo<'info>,
    t: &DelayedSale,
    events: &AccountInfo<'info>,
) -> Result<()> {
    m.delayed_shares = m.delayed_shares.checked_sub(t.p_amount).ok_or(ExchangeError::MathOverflow)?;
    shares.send_held(held, dest, t.p_amount)?;
    crate::events::emit(events, &Cancelled { ticket: t.ticket })
}

/// Burn pToken the exchange holds beyond delayed-sale stock: everything in
/// `held` above `delayed_shares`, and all of `other`, any other pToken account
/// the exchange PDA owns (on the EVM every pToken sent to the exchange's
/// address is absorbable; here the natural "send to the exchange" is the
/// config PDA's associated token account). Curves send the float share of
/// their fees here; burning lowers supply, the desk sells the matching
/// shares. The USDC they were bought with already sits in the float.
pub fn absorb<'info>(
    config: &mut Config,
    m: &mut Market,
    shares: &Shares<'info>,
    held: &AccountInfo<'info>,
    other: Option<&AccountInfo<'info>>,
    events: &AccountInfo<'info>,
    now: i64,
) -> Result<u64> {
    let from_held = token_amount(held)?.checked_sub(m.delayed_shares).ok_or(ExchangeError::MathOverflow)?;
    let from_other = match other {
        Some(ai) => token_amount(ai)?,
        None => 0,
    };
    let amount = from_held.checked_add(from_other).ok_or(ExchangeError::MathOverflow)?;
    if amount == 0 {
        // Nothing to burn, but direct burns are counted as on every visit.
        config.burned_outside(m, mint_supply(&shares.mint)?);
        return Ok(0);
    }
    if from_held > 0 {
        shares.burn_held(held, from_held)?;
    }
    if let Some(ai) = other.filter(|_| from_other > 0) {
        shares.burn_held(ai, from_other)?;
    }
    let supply = mint_supply(&shares.mint)?;
    config.burned(m, amount, supply, now)?;
    crate::events::emit(events, &Absorbed { position_id: m.position_id, amount })?;
    Ok(amount)
}
