//! The float: token movements signed by the exchange PDA, the redemption
//! queue and the payment path every redemption and release goes through.
//!
//! Solana has no try/catch around a CPI: a failed transfer aborts the whole
//! transaction. Where the EVM tries a transfer and sets the claim aside if it
//! fails (`_tryTransfer`), this checks the recipient account's state first
//! (`recipient_usable`) and only transfers to accounts that will accept it.
//! For classic SPL Token USDC the only ways a transfer to a token account can
//! fail are: missing or not a token account, wrong mint, or frozen. A frozen
//! float vault is the "fails for everyone" case (the EVM's paused USDG).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::system_program;
use anchor_spl::token::{self, spl_token, Burn, MintTo, TransferChecked};

use crate::error::ExchangeError;
use crate::events::{self, ClaimPaid, ClaimUndelivered, Queued, SetAside};
use crate::state::*;

/// Token amount of a token account, read fresh from its data (after CPIs).
pub fn token_amount(ai: &AccountInfo) -> Result<u64> {
    token::accessor::amount(ai)
}

/// Supply of a mint, read fresh from its data (after CPIs).
pub fn mint_supply(ai: &AccountInfo) -> Result<u64> {
    let data = ai.try_borrow_data()?;
    let m = spl_token::state::Mint::unpack(&data)?;
    Ok(m.supply)
}

fn frozen(ai: &AccountInfo) -> Result<bool> {
    let data = ai.try_borrow_data()?;
    let a = spl_token::state::Account::unpack(&data)?;
    Ok(a.state == spl_token::state::AccountState::Frozen)
}

/// Whether a USDC transfer to `ai` would succeed: an initialized classic SPL
/// Token account for `usdc_mint` that is not frozen, and not the float itself.
pub fn recipient_usable(ai: &AccountInfo, usdc_mint: &Pubkey, vault: &Pubkey) -> bool {
    if ai.key == vault || *ai.owner != spl_token::ID || ai.data_len() != spl_token::state::Account::LEN {
        return false;
    }
    let Ok(data) = ai.try_borrow_data() else { return false };
    match spl_token::state::Account::unpack(&data) {
        Ok(a) => a.mint == *usdc_mint && a.state == spl_token::state::AccountState::Initialized,
        Err(_) => false,
    }
}

/// Create a program-owned PDA at `target` (checked against `seeds`), paid by
/// `payer`. Works when someone has pre-funded the address, so nobody can
/// block the next claim or ticket by sending lamports to it.
pub fn create_pda<'info>(
    payer: &AccountInfo<'info>,
    target: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    seeds: &[&[u8]],
    space: usize,
) -> Result<u8> {
    let (addr, bump) = Pubkey::find_program_address(seeds, &crate::ID);
    require_keys_eq!(addr, *target.key, ExchangeError::WrongAccount);
    let bump_bytes = [bump];
    let mut signer: Vec<&[u8]> = seeds.to_vec();
    signer.push(&bump_bytes);
    let signer = [signer.as_slice()];
    let rent = Rent::get()?.minimum_balance(space);
    let have = target.lamports();
    if have == 0 {
        system_program::create_account(
            CpiContext::new_with_signer(
                system.key(),
                system_program::CreateAccount { from: payer.clone(), to: target.clone() },
                &signer,
            ),
            rent,
            space as u64,
            &crate::ID,
        )?;
    } else {
        // Only an empty system account can be taken over this way.
        require_keys_eq!(*target.owner, system_program::ID, ExchangeError::WrongAccount);
        require!(target.data_is_empty(), ExchangeError::WrongAccount);
        if have < rent {
            system_program::transfer(
                CpiContext::new(system.key(), system_program::Transfer { from: payer.clone(), to: target.clone() }),
                rent - have,
            )?;
        }
        system_program::allocate(
            CpiContext::new_with_signer(system.key(), system_program::Allocate { account_to_allocate: target.clone() }, &signer),
            space as u64,
        )?;
        system_program::assign(
            CpiContext::new_with_signer(system.key(), system_program::Assign { account_to_assign: target.clone() }, &signer),
            &crate::ID,
        )?;
    }
    Ok(bump)
}

/// Write a freshly created account's data (discriminator included).
pub fn write_account<T: AccountSerialize>(target: &AccountInfo, value: &T) -> Result<()> {
    let mut data = target.try_borrow_mut_data()?;
    let mut cursor: &mut [u8] = &mut data;
    value.try_serialize(&mut cursor)
}

/// Close a program-owned account, its rent to `to`. Only after the
/// instruction's last CPI: the runtime checks that lamports balance across
/// every account a CPI is given, and a CPI that gets `to` (a seller paying
/// for a new claim it was also the rent payer of a paid one, say) but not the
/// closed account would see lamports appear from nowhere and fail. Payments
/// queue their closes on `Float` and `Float::finish` makes them.
pub fn close_account<'info>(account: &AccountInfo<'info>, to: &AccountInfo<'info>) -> Result<()> {
    let lamports = account.lamports();
    **to.try_borrow_mut_lamports()? = to.lamports().checked_add(lamports).ok_or(ExchangeError::MathOverflow)?;
    **account.try_borrow_mut_lamports()? = 0;
    account.assign(&system_program::ID);
    account.resize(0)?;
    Ok(())
}

/// Accounts a payment may create: the next queue claim (`["claim",
/// claim_tail]`) and the next set-aside claim (`["aside", aside_next]`).
/// The client passes both addresses; each is created only if needed.
pub struct Slots<'a, 'info> {
    pub payer: AccountInfo<'info>,
    pub system_program: AccountInfo<'info>,
    pub claim: Option<&'a AccountInfo<'info>>,
    pub aside: Option<&'a AccountInfo<'info>>,
}

/// Everything that moves USDC out of the float, with the config it keeps.
pub struct Float<'a, 'info> {
    pub config: &'a mut Config,
    /// The config PDA: authority of the vault and every pToken mint.
    pub authority: AccountInfo<'info>,
    pub vault: AccountInfo<'info>,
    pub usdc_mint: AccountInfo<'info>,
    pub usdc_decimals: u8,
    pub token_program: AccountInfo<'info>,
    pub event_authority: AccountInfo<'info>,
    /// Paid claims to close, with where their rent goes, once no CPI follows.
    pub closes: Vec<(AccountInfo<'info>, AccountInfo<'info>)>,
}

impl<'a, 'info> Float<'a, 'info> {
    /// Close the claims paid during this instruction. Every handler that
    /// pays calls it last, after its final CPI (see `close_account`).
    pub fn finish(&mut self) -> Result<()> {
        for (claim, rent_to) in self.closes.drain(..) {
            close_account(&claim, &rent_to)?;
        }
        Ok(())
    }

    pub fn vault_amount(&self) -> Result<u64> {
        token_amount(&self.vault)
    }

    pub fn free_float(&self) -> Result<u64> {
        Ok(self.config.free_float(self.vault_amount()?))
    }

    pub fn emit<E: anchor_lang::Event>(&self, e: &E) -> Result<()> {
        events::emit(&self.event_authority, e)
    }

    /// Transfer USDC out of the float, signed by the exchange PDA.
    pub fn send(&self, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
        let bump = [self.config.bump];
        let seeds: &[&[u8]] = &[CONFIG_SEED, &bump];
        token::transfer_checked(
            CpiContext::new_with_signer(
                self.token_program.key(),
                TransferChecked {
                    from: self.vault.clone(),
                    mint: self.usdc_mint.clone(),
                    to: to.clone(),
                    authority: self.authority.clone(),
                },
                &[seeds],
            ),
            amount,
            self.usdc_decimals,
        )
    }

    /// Pay up to `max` queued claims, oldest first, while the float covers
    /// them (`_payQueue`). `accounts` holds `(claim, recipient, rent_to)`
    /// triples for `claim_head, claim_head + 1, ...`; fewer may be passed,
    /// which only pays fewer: a claim's USDC is reserved in `queued` either way.
    ///
    /// With `head_required` (the batch every mint and payment runs, as the
    /// EVM's `_payQueue(3)` runs unconditionally), a live queue must get its
    /// head claim's triple, so no client can buy or sell past the queue
    /// without at least offering to pay it. Claims paid or set aside since
    /// the client read the queue are skipped, so a list of the next three
    /// still works unless all three went meanwhile. A closed account cannot
    /// stand in for the head: the program cannot tell a claim it paid from
    /// any other empty account, and accepting one would let anyone skip the
    /// queue. Clients resend with a fresh list on `MissingAccount`.
    pub fn pay_queue(&mut self, max: usize, accounts: &'info [AccountInfo<'info>], head_required: bool) -> Result<u64> {
        let mut head = self.config.claim_head;
        let tail = self.config.claim_tail;
        // Set-aside claims keep their USDC; the queue is paid from the rest.
        let mut bal = self.vault_amount()?.saturating_sub(self.config.unclaimed_total);
        let vault_frozen = frozen(&self.vault)?;
        let mut paid = 0u64;
        let mut saw_head = head >= tail;
        for triple in accounts.chunks_exact(3).take(max) {
            if head >= tail {
                break;
            }
            let (claim_ai, recipient, rent_to) = (&triple[0], &triple[1], &triple[2]);
            // A claim account the program no longer owns was paid and closed;
            // one behind the head was set aside meanwhile. Both are done with.
            if *claim_ai.owner != crate::ID {
                continue;
            }
            let mut claim: Account<'info, Claim> = Account::try_from(claim_ai)?;
            if claim.index < head && claim.set_aside {
                continue;
            }
            require!(claim.index == head && !claim.set_aside, ExchangeError::QueueOutOfOrder);
            saw_head = true;
            require_keys_eq!(*claim_ai.key, Pubkey::create_program_address(&[CLAIM_SEED, &head.to_le_bytes(), &[claim.bump]], &crate::ID).map_err(|_| ExchangeError::WrongAccount)?, ExchangeError::WrongAccount);
            require_keys_eq!(*recipient.key, claim.to, ExchangeError::WrongAccount);
            require_keys_eq!(*rent_to.key, claim.payer, ExchangeError::WrongAccount);
            // Strict FIFO: a claim the float cannot cover blocks those behind it.
            if claim.amount > bal {
                break;
            }
            // Every transfer failing (the float frozen) is not this
            // recipient's fault: stop, and keep the queue in order.
            if vault_frozen {
                break;
            }
            let amount = claim.amount;
            // Either way the amount is spoken for: paid out, or reserved for its owner.
            bal -= amount;
            if recipient_usable(recipient, self.usdc_mint.key, self.vault.key) {
                self.config.queued = self.config.queued.checked_sub(amount).ok_or(ExchangeError::MathOverflow)?;
                self.send(recipient, amount)?;
                self.closes.push((claim_ai.clone(), rent_to.clone()));
                self.emit(&ClaimPaid { ticket: head, to: claim.to, amount })?;
            } else {
                // Still owed and still counted in `queued`, but no longer in line.
                claim.set_aside = true;
                claim.exit(&crate::ID)?;
                self.config.unclaimed_total = self.config.unclaimed_total.checked_add(amount).ok_or(ExchangeError::MathOverflow)?;
                self.emit(&ClaimUndelivered { ticket: head, to: claim.to, amount })?;
            }
            head += 1;
            paid += 1;
        }
        require!(saw_head || !head_required, ExchangeError::MissingAccount);
        self.config.claim_head = head;
        Ok(paid)
    }

    /// Pay `amount` to `to` from the float now, queueing whatever it cannot
    /// cover (`_pay`). With `set_aside` (a release, which the seller did not
    /// time), a recipient that cannot be paid has the payment kept for
    /// `withdraw_unclaimed`, as the queue does; a frozen float reverts.
    pub fn pay(
        &mut self,
        to: &AccountInfo<'info>,
        amount: u64,
        set_aside: bool,
        slots: &mut Slots<'_, 'info>,
        queue: &'info [AccountInfo<'info>],
    ) -> Result<()> {
        if self.config.queued > 0 {
            self.pay_queue(QUEUE_BATCH, queue, true)?;
        }
        let free = self.free_float()?;
        let now_ = amount.min(free);
        if now_ > 0 {
            if !set_aside || recipient_usable(to, self.usdc_mint.key, self.vault.key) {
                // A direct sale reverts if its own recipient cannot be paid;
                // so does any payment while the float itself is frozen.
                self.send(to, now_)?;
            } else {
                require!(!frozen(&self.vault)?, ExchangeError::FloatFrozen);
                let index = self.config.aside_next;
                let slot = slots.aside.ok_or(ExchangeError::MissingAccount)?;
                self.push_claim(slot, slots, ASIDE_SEED, index, *to.key, now_, true)?;
                self.config.aside_next += 1;
                self.config.unclaimed_total = self.config.unclaimed_total.checked_add(now_).ok_or(ExchangeError::MathOverflow)?;
                self.config.queued = self.config.queued.checked_add(now_).ok_or(ExchangeError::MathOverflow)?;
                self.emit(&SetAside { claim: *slot.key, to: *to.key, amount: now_ })?;
            }
        }
        if amount > now_ {
            let rest = amount - now_;
            let index = self.config.claim_tail;
            let slot = slots.claim.ok_or(ExchangeError::MissingAccount)?;
            self.push_claim(slot, slots, CLAIM_SEED, index, *to.key, rest, false)?;
            self.config.claim_tail += 1;
            self.config.queued = self.config.queued.checked_add(rest).ok_or(ExchangeError::MathOverflow)?;
            self.emit(&Queued { ticket: index, to: *to.key, amount: rest })?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_claim(
        &self,
        slot: &AccountInfo<'info>,
        slots: &Slots<'_, 'info>,
        prefix: &[u8],
        index: u64,
        to: Pubkey,
        amount: u64,
        set_aside: bool,
    ) -> Result<()> {
        let idx = index.to_le_bytes();
        let bump = create_pda(&slots.payer, slot, &slots.system_program, &[prefix, &idx], 8 + Claim::INIT_SPACE)?;
        write_account(slot, &Claim { index, to, amount, payer: *slots.payer.key, set_aside, bump })
    }
}

/// pToken movements signed by the exchange PDA.
pub struct Shares<'info> {
    pub authority: AccountInfo<'info>,
    pub bump: u8,
    pub mint: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
}

impl<'info> Shares<'info> {
    pub fn mint_to(&self, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
        let bump = [self.bump];
        let seeds: &[&[u8]] = &[CONFIG_SEED, &bump];
        token::mint_to(
            CpiContext::new_with_signer(
                self.token_program.key(),
                MintTo { mint: self.mint.clone(), to: to.clone(), authority: self.authority.clone() },
                &[seeds],
            ),
            amount,
        )
    }

    /// Burn from an account the exchange PDA owns (`held`, or another one `absorb` was given).
    pub fn burn_held(&self, from: &AccountInfo<'info>, amount: u64) -> Result<()> {
        let bump = [self.bump];
        let seeds: &[&[u8]] = &[CONFIG_SEED, &bump];
        token::burn(
            CpiContext::new_with_signer(
                self.token_program.key(),
                Burn { mint: self.mint.clone(), from: from.clone(), authority: self.authority.clone() },
                &[seeds],
            ),
            amount,
        )
    }

    /// Transfer out of an account the exchange PDA owns (`held`).
    pub fn send_held(&self, from: &AccountInfo<'info>, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
        let bump = [self.bump];
        let seeds: &[&[u8]] = &[CONFIG_SEED, &bump];
        token::transfer_checked(
            CpiContext::new_with_signer(
                self.token_program.key(),
                TransferChecked { from: from.clone(), mint: self.mint.clone(), to: to.clone(), authority: self.authority.clone() },
                &[seeds],
            ),
            amount,
            PTOKEN_DECIMALS,
        )
    }
}
