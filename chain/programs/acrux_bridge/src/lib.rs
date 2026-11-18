//! The Acrux bridge on Solana. A deposit locks SPL tokens (SPL Token or Token-2022) in the bridge's
//! vault for their mint and writes a deposit record that Acrux's validators read once it is
//! finalized: the Acrux address to credit, the mint and the amount the vault received. Every
//! deposit takes the next sequence number, so a record is credited on Acrux exactly once.
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5");

#[program]
pub mod acrux_bridge {
    use super::*;

    /// Creates the bridge's one state account, which numbers the deposits. Anyone may send it once.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        ctx.accounts.bridge.set_inner(Bridge { deposits: 0, bump: ctx.bumps.bridge });
        Ok(())
    }

    /// Locks `amount` of the depositor's tokens in the mint's vault and records the deposit for
    /// `recipient` on Acrux. The record holds what the vault received, after any Token-2022
    /// transfer fee.
    pub fn deposit(ctx: Context<Deposit>, amount: u64, recipient: Pubkey) -> Result<()> {
        require!(amount > 0, BridgeError::ZeroAmount);
        let before = ctx.accounts.vault.amount;
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.source.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;
        ctx.accounts.vault.reload()?;
        let received = ctx.accounts.vault.amount - before;
        require!(received > 0, BridgeError::ZeroAmount);

        let bridge = &mut ctx.accounts.bridge;
        let sequence = bridge.deposits;
        bridge.deposits += 1;
        let slot = Clock::get()?.slot;
        ctx.accounts.record.set_inner(DepositRecord {
            sequence,
            depositor: ctx.accounts.depositor.key(),
            recipient,
            mint: ctx.accounts.mint.key(),
            amount: received,
            slot,
            bump: ctx.bumps.record,
        });
        emit_cpi!(Deposited { sequence, depositor: ctx.accounts.depositor.key(), recipient, mint: ctx.accounts.mint.key(), amount: received, slot });
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Bridge {
    /// Deposits made so far; the next deposit's sequence number.
    pub deposits: u64,
    pub bump: u8,
}

/// One deposit, at `["deposit", sequence]`.
#[account]
#[derive(InitSpace)]
pub struct DepositRecord {
    pub sequence: u64,
    pub depositor: Pubkey,
    /// The Acrux address the deposit is credited to.
    pub recipient: Pubkey,
    pub mint: Pubkey,
    /// What the vault received, in the mint's base units.
    pub amount: u64,
    /// The Solana slot the deposit landed in.
    pub slot: u64,
    pub bump: u8,
}

#[event]
pub struct Deposited {
    pub sequence: u64,
    pub depositor: Pubkey,
    pub recipient: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub slot: u64,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(init, payer = payer, space = 8 + Bridge::INIT_SPACE, seeds = [b"bridge"], bump)]
    pub bridge: Account<'info, Bridge>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,
    #[account(mut, seeds = [b"bridge"], bump = bridge.bump)]
    pub bridge: Account<'info, Bridge>,
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = depositor, token::token_program = token_program)]
    pub source: InterfaceAccount<'info, TokenAccount>,
    /// The mint's vault: a token account at `["vault", mint]` that is its own authority, created
    /// by the mint's first deposit.
    #[account(
        init_if_needed,
        payer = depositor,
        seeds = [b"vault", mint.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = vault,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init,
        payer = depositor,
        space = 8 + DepositRecord::INIT_SPACE,
        seeds = [b"deposit", bridge.deposits.to_le_bytes().as_ref()],
        bump,
    )]
    pub record: Account<'info, DepositRecord>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[error_code]
pub enum BridgeError {
    #[msg("A deposit moves more than nothing.")]
    ZeroAmount,
}
