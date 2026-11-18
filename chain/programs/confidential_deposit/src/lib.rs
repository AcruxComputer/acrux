//! A deposit adapter for the Acrux bridge.
//!
//! The bridge records who deposited (`DepositRecord.depositor`) next to the Acrux address that is
//! credited. When a wallet deposits directly, that record ties the wallet to the recipient.
//! This adapter puts a program-owned escrow between the two: the user's tokens go into a shared
//! escrow token account, and the adapter's PDA, not the user, is the bridge's depositor.
//!
//! What this protects: the bridge's `DepositRecord` and `Deposited` event name the adapter PDA as
//! depositor, so reading the bridge alone does not link the recipient to the user's wallet.
//!
//! What it does NOT protect: the user's transfer into the escrow, its amount and its slot are
//! public on Solana, and so is the bridge deposit that follows in the same transaction. Anyone who
//! reads the transaction sees wallet, amount and recipient together. This is not anonymity. See
//! `docs/confidential-deposit.md`.
use acrux_bridge::program::AcruxBridge;
use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer as sol_transfer, Transfer as SolTransfer};
use anchor_spl::token_interface::{transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("5Gq3z6Vx1oYk3d2s8mN7bWc4tHfRjLpA9uEeXyZkQvDa");

/// Seed of the adapter's signing PDA: the bridge's depositor.
pub const ADAPTER_SEED: &[u8] = b"adapter";
/// Seed of the escrow token account for a mint.
pub const ESCROW_SEED: &[u8] = b"escrow";
/// Size of a base SPL token account (no Token-2022 extensions).
pub const TOKEN_ACCOUNT_SIZE: usize = 165;
/// Size of the bridge's `DepositRecord` account: 8 (discriminator) + 8 + 32 + 32 + 32 + 8 + 8 + 1.
pub const DEPOSIT_RECORD_SIZE: usize = 8 + 8 + 32 + 32 + 32 + 8 + 8 + 1;

/// Lamports the adapter PDA must receive so it can pay the bridge's account rent: the deposit
/// record, and the mint's vault when that does not exist yet. Pure, so it is testable natively.
pub fn rent_topup(rent: &Rent, vault_exists: bool, adapter_lamports: u64) -> u64 {
    let mut needed = rent.minimum_balance(DEPOSIT_RECORD_SIZE);
    if !vault_exists {
        needed += rent.minimum_balance(TOKEN_ACCOUNT_SIZE);
    }
    // The PDA is a system account holding no data, so it must also stay at zero rent-exempt
    // minimum or above after paying.
    let floor = rent.minimum_balance(0);
    (needed + floor).saturating_sub(adapter_lamports)
}

/// Checks a recipient before it is written into a public record. The all-zero key is the system
/// program's address and nobody holds it, so tokens credited there would be lost. A recipient that
/// is the depositing wallet itself defeats the adapter's purpose.
pub fn check_recipient(recipient: &Pubkey, depositor: &Pubkey) -> Result<()> {
    require!(*recipient != Pubkey::default(), AdapterError::BadRecipient);
    require!(recipient != depositor, AdapterError::RecipientIsDepositor);
    Ok(())
}

/// What the escrow received from a transfer, from its balance before and after. A Token-2022
/// transfer fee makes this smaller than the requested amount. A balance that fell, or did not move,
/// is an error: nothing may be forwarded to the bridge.
pub fn received_amount(before: u64, after: u64) -> Result<u64> {
    let received = after.checked_sub(before).ok_or(AdapterError::EscrowBalanceMismatch)?;
    require!(received > 0, AdapterError::ZeroAmount);
    Ok(received)
}

/// After the bridge call the escrow must be back at its balance from before this deposit.
pub fn check_drained(before: u64, after: u64) -> Result<()> {
    require!(after == before, AdapterError::EscrowNotDrained);
    Ok(())
}

#[program]
pub mod confidential_deposit {
    use super::*;

    /// Moves `amount` from the user's token account into the mint's escrow, then deposits exactly
    /// that amount from the escrow into the bridge with the adapter PDA as depositor.
    /// `recipient` is the Acrux address to credit; use a fresh key made for this deposit.
    pub fn deposit(ctx: Context<AdapterDeposit>, amount: u64, recipient: Pubkey) -> Result<()> {
        require!(amount > 0, AdapterError::ZeroAmount);
        check_recipient(&recipient, &ctx.accounts.user.key())?;

        // 1. The user pays the adapter PDA the rent the bridge will charge its depositor.
        let vault_exists = ctx.accounts.vault.lamports() > 0;
        let topup = rent_topup(&Rent::get()?, vault_exists, ctx.accounts.adapter.lamports());
        if topup > 0 {
            sol_transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    SolTransfer { from: ctx.accounts.user.to_account_info(), to: ctx.accounts.adapter.to_account_info() },
                ),
                topup,
            )?;
        }

        // 2. The user's tokens move into the escrow.
        let before = ctx.accounts.escrow.amount;
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.source.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.escrow.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;
        ctx.accounts.escrow.reload()?;
        // What the escrow received, after any Token-2022 transfer fee. Only this is forwarded,
        // so other users' funds in the escrow are never touched.
        let received = received_amount(before, ctx.accounts.escrow.amount)?;

        // 3. The adapter PDA deposits that amount into the bridge.
        let bump = ctx.bumps.adapter;
        let seeds: &[&[u8]] = &[ADAPTER_SEED, &[bump]];
        let signer = &[seeds];
        let cpi = CpiContext::new_with_signer(
            ctx.accounts.bridge_program.to_account_info(),
            acrux_bridge::cpi::accounts::Deposit {
                depositor: ctx.accounts.adapter.to_account_info(),
                bridge: ctx.accounts.bridge.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                source: ctx.accounts.escrow.to_account_info(),
                vault: ctx.accounts.vault.to_account_info(),
                record: ctx.accounts.record.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
                event_authority: ctx.accounts.bridge_event_authority.to_account_info(),
                program: ctx.accounts.bridge_program.to_account_info(),
            },
            signer,
        );
        acrux_bridge::cpi::deposit(cpi, received, recipient)?;

        // The escrow must be empty of this deposit again: nothing is left parked in it.
        ctx.accounts.escrow.reload()?;
        check_drained(before, ctx.accounts.escrow.amount)?;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct AdapterDeposit<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    /// The adapter's signing PDA. It holds no data and is the bridge's depositor.
    /// CHECK: a system-owned PDA, derived from the adapter seed.
    #[account(mut, seeds = [ADAPTER_SEED], bump)]
    pub adapter: UncheckedAccount<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = user, token::token_program = token_program)]
    pub source: InterfaceAccount<'info, TokenAccount>,
    /// The shared escrow for this mint, owned by the adapter PDA, created by its first use.
    #[account(
        init_if_needed,
        payer = user,
        seeds = [ESCROW_SEED, mint.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = adapter,
        token::token_program = token_program,
    )]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: the bridge's state account; the bridge checks its seeds.
    #[account(mut)]
    pub bridge: UncheckedAccount<'info>,
    /// CHECK: the bridge's vault for the mint; the bridge checks its seeds.
    #[account(mut)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: the next deposit record; the bridge checks its seeds.
    #[account(mut)]
    pub record: UncheckedAccount<'info>,
    /// CHECK: the bridge's event authority; the bridge checks it.
    pub bridge_event_authority: UncheckedAccount<'info>,
    pub bridge_program: Program<'info, AcruxBridge>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[error_code]
pub enum AdapterError {
    #[msg("amount must be greater than zero")]
    ZeroAmount,
    #[msg("recipient is not a usable Acrux address")]
    BadRecipient,
    #[msg("recipient is the depositing wallet; use a fresh address")]
    RecipientIsDepositor,
    #[msg("escrow balance fell or is inconsistent after the transfer")]
    EscrowBalanceMismatch,
    #[msg("escrow still holds this deposit after the bridge call")]
    EscrowNotDrained,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rent() -> Rent {
        Rent::default()
    }

    #[test]
    fn record_size_matches_the_bridge() {
        use anchor_lang::Space;
        assert_eq!(DEPOSIT_RECORD_SIZE, 8 + acrux_bridge::DepositRecord::INIT_SPACE);
    }

    #[test]
    fn topup_covers_record_and_new_vault() {
        let r = rent();
        let t = rent_topup(&r, false, 0);
        assert_eq!(t, r.minimum_balance(DEPOSIT_RECORD_SIZE) + r.minimum_balance(TOKEN_ACCOUNT_SIZE) + r.minimum_balance(0));
    }

    #[test]
    fn topup_skips_the_vault_once_it_exists() {
        let r = rent();
        let t = rent_topup(&r, true, 0);
        assert_eq!(t, r.minimum_balance(DEPOSIT_RECORD_SIZE) + r.minimum_balance(0));
    }

    #[test]
    fn topup_is_zero_when_the_pda_is_already_funded() {
        let r = rent();
        assert_eq!(rent_topup(&r, true, 10_000_000), 0);
    }

    #[test]
    fn topup_counts_what_the_pda_already_holds() {
        let r = rent();
        let full = rent_topup(&r, true, 0);
        assert_eq!(rent_topup(&r, true, 1_000), full - 1_000);
    }

    #[test]
    fn recipient_checks() {
        let user = Pubkey::new_unique();
        assert!(check_recipient(&Pubkey::new_unique(), &user).is_ok());
        assert!(check_recipient(&Pubkey::default(), &user).is_err());
        assert!(check_recipient(&user, &user).is_err());
    }

    #[test]
    fn pdas_are_derived_from_fixed_seeds() {
        let (adapter, _) = Pubkey::find_program_address(&[ADAPTER_SEED], &crate::ID);
        let mint = Pubkey::new_unique();
        let (escrow, _) = Pubkey::find_program_address(&[ESCROW_SEED, mint.as_ref()], &crate::ID);
        let (other, _) = Pubkey::find_program_address(&[ESCROW_SEED, Pubkey::new_unique().as_ref()], &crate::ID);
        assert_ne!(adapter, escrow);
        assert_ne!(escrow, other);
        // The escrow is per mint, never per user: nothing in its address names the wallet.
    }

    #[test]
    fn bridge_call_names_the_adapter_and_never_the_user() {
        use anchor_lang::{InstructionData, ToAccountMetas};
        let (adapter, _) = Pubkey::find_program_address(&[ADAPTER_SEED], &crate::ID);
        let user = Pubkey::new_unique();
        let k = Pubkey::new_unique;
        let accounts = acrux_bridge::accounts::Deposit {
            depositor: adapter,
            bridge: k(),
            mint: k(),
            source: k(),
            vault: k(),
            record: k(),
            token_program: k(),
            system_program: k(),
            event_authority: k(),
            program: acrux_bridge::ID,
        };
        let metas = accounts.to_account_metas(None);
        assert_eq!(metas[0].pubkey, adapter);
        assert!(metas[0].is_signer && metas[0].is_writable);
        assert!(metas.iter().all(|m| m.pubkey != user));
        let recipient = Pubkey::new_unique();
        let data = acrux_bridge::instruction::Deposit { amount: 7, recipient }.data();
        // 8-byte discriminator, u64 amount, 32-byte recipient.
        assert_eq!(data.len(), 8 + 8 + 32);
        assert_eq!(&data[8..16], &7u64.to_le_bytes());
        assert_eq!(&data[16..], recipient.as_ref());
    }

    fn code(e: anchor_lang::error::Error) -> u32 {
        match e {
            anchor_lang::error::Error::AnchorError(a) => a.error_code_number,
            _ => 0,
        }
    }
    fn err_of(e: AdapterError) -> u32 {
        anchor_lang::error::ERROR_CODE_OFFSET + e as u32
    }

    #[test]
    fn received_amount_full_when_no_fee() {
        assert_eq!(received_amount(500, 1_500).unwrap(), 1_000);
    }

    #[test]
    fn received_amount_is_net_of_a_transfer_fee() {
        // 1_000 requested, 1% Token-2022 fee withheld: only 990 arrives and only 990 is forwarded.
        assert_eq!(received_amount(0, 990).unwrap(), 990);
        // Other users' funds already in the escrow are not counted.
        assert_eq!(received_amount(7_000, 7_990).unwrap(), 990);
    }

    #[test]
    fn received_amount_rejects_zero_and_negative() {
        // A fee that eats the whole amount, or a transfer that moved nothing.
        assert_eq!(code(received_amount(10, 10).unwrap_err()), err_of(AdapterError::ZeroAmount));
        // The escrow lost funds during the transfer: never forward.
        assert_eq!(code(received_amount(10, 9).unwrap_err()), err_of(AdapterError::EscrowBalanceMismatch));
    }

    #[test]
    fn received_amount_handles_extremes() {
        assert_eq!(received_amount(0, u64::MAX).unwrap(), u64::MAX);
        assert_eq!(received_amount(u64::MAX - 1, u64::MAX).unwrap(), 1);
        assert!(received_amount(u64::MAX, 0).is_err());
    }

    #[test]
    fn drained_check_catches_leftover_and_overdraw() {
        assert!(check_drained(500, 500).is_ok());
        // Bridge took less than forwarded: tokens stuck in escrow.
        assert_eq!(code(check_drained(500, 900).unwrap_err()), err_of(AdapterError::EscrowNotDrained));
        // Bridge took more than forwarded: other users' funds touched.
        assert_eq!(code(check_drained(500, 100).unwrap_err()), err_of(AdapterError::EscrowNotDrained));
    }

    #[test]
    fn recipient_error_codes_are_distinct() {
        let u = Pubkey::new_unique();
        assert_eq!(code(check_recipient(&Pubkey::default(), &u).unwrap_err()), err_of(AdapterError::BadRecipient));
        assert_eq!(code(check_recipient(&u, &u).unwrap_err()), err_of(AdapterError::RecipientIsDepositor));
    }

    #[test]
    fn topup_never_underflows_and_never_overflows() {
        let r = rent();
        assert_eq!(rent_topup(&r, false, u64::MAX), 0);
        assert!(rent_topup(&r, false, 0) > rent_topup(&r, true, 0));
    }

    #[test]
    fn adapter_pda_is_off_curve_so_only_this_program_can_sign() {
        let (adapter, _) = Pubkey::find_program_address(&[ADAPTER_SEED], &crate::ID);
        assert!(!adapter.is_on_curve());
        let mint = Pubkey::new_unique();
        let (escrow, _) = Pubkey::find_program_address(&[ESCROW_SEED, mint.as_ref()], &crate::ID);
        assert!(!escrow.is_on_curve());
    }

    #[test]
    fn escrow_address_is_per_mint_and_unrelated_to_the_bridge_vault() {
        let mint = Pubkey::new_unique();
        let (escrow, _) = Pubkey::find_program_address(&[ESCROW_SEED, mint.as_ref()], &crate::ID);
        let (vault, _) = Pubkey::find_program_address(&[b"vault", mint.as_ref()], &acrux_bridge::ID);
        assert_ne!(escrow, vault);
        // A different mint gives a different escrow: a wrong-mint escrow cannot match the mint seed.
        let (wrong, _) = Pubkey::find_program_address(&[ESCROW_SEED, Pubkey::new_unique().as_ref()], &crate::ID);
        assert_ne!(escrow, wrong);
    }

    #[test]
    fn bridge_deposit_discriminator_is_stable() {
        use anchor_lang::Discriminator;
        let d = acrux_bridge::instruction::Deposit::DISCRIMINATOR;
        let expect = &anchor_lang::solana_program::hash::hash(b"global:deposit").to_bytes()[..8];
        assert_eq!(d, expect);
    }
}
