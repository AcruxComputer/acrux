//! The Acrux Net demo pool: one constant-product pool over two mints.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("CUBR9LNdTwxwp5ARBQj1EopfuH8dr8u4i7DsXcb8HbHN");

pub const POOL_SEED: &[u8] = b"pool";
pub const VAULT_SEED: &[u8] = b"vault";
pub const BPS: u64 = 10_000;

#[program]
pub mod demo_pool {
    use super::*;

    /// Creates the pool for a pair of mints and funds its vaults from the payer.
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        fee_bps: u16,
        amount_a: u64,
        amount_b: u64,
    ) -> Result<()> {
        require!((fee_bps as u64) < BPS, PoolError::InvalidFee);
        require!(amount_a > 0 && amount_b > 0, PoolError::ZeroAmount);
        ctx.accounts.pool.set_inner(Pool {
            mint_a: ctx.accounts.mint_a.key(),
            mint_b: ctx.accounts.mint_b.key(),
            vault_a: ctx.accounts.vault_a.key(),
            vault_b: ctx.accounts.vault_b.key(),
            fee_bps,
            bump: ctx.bumps.pool,
        });
        for (from, to, amount) in [
            (&ctx.accounts.payer_a, &ctx.accounts.vault_a, amount_a),
            (&ctx.accounts.payer_b, &ctx.accounts.vault_b, amount_b),
        ] {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: from.to_account_info(),
                        to: to.to_account_info(),
                        authority: ctx.accounts.payer.to_account_info(),
                    },
                ),
                amount,
            )?;
        }
        Ok(())
    }

    /// Swaps exactly `amount_in` of one mint for the other, A to B when `a_to_b`.
    /// The fee is taken from the input and stays in the pool. Fails when the output
    /// would be under `minimum_out`.
    pub fn swap(ctx: Context<Swap>, amount_in: u64, minimum_out: u64, a_to_b: bool) -> Result<()> {
        let a = &ctx.accounts;
        let (vault_in, vault_out, user_in, user_out) = if a_to_b {
            (&a.vault_a, &a.vault_b, &a.user_a, &a.user_b)
        } else {
            (&a.vault_b, &a.vault_a, &a.user_b, &a.user_a)
        };
        let amount_out = quote(vault_in.amount, vault_out.amount, a.pool.fee_bps, amount_in)?;
        require!(amount_out >= minimum_out, PoolError::SlippageExceeded);

        token::transfer(
            CpiContext::new(
                a.token_program.to_account_info(),
                Transfer {
                    from: user_in.to_account_info(),
                    to: vault_in.to_account_info(),
                    authority: a.user.to_account_info(),
                },
            ),
            amount_in,
        )?;
        token::transfer(
            CpiContext::new_with_signer(
                a.token_program.to_account_info(),
                Transfer {
                    from: vault_out.to_account_info(),
                    to: user_out.to_account_info(),
                    authority: a.pool.to_account_info(),
                },
                &[&[POOL_SEED, a.pool.mint_a.as_ref(), a.pool.mint_b.as_ref(), &[a.pool.bump]]],
            ),
            amount_out,
        )
    }
}

/// fee = ceil(amount_in * fee_bps / 10000), net = amount_in - fee,
/// out = floor(reserve_out * net / (reserve_in + net)).
pub fn quote(reserve_in: u64, reserve_out: u64, fee_bps: u16, amount_in: u64) -> Result<u64> {
    let fee = (amount_in as u128 * fee_bps as u128).div_ceil(BPS as u128);
    let net = amount_in as u128 - fee;
    require!(net > 0, PoolError::ZeroAmount);
    let out = reserve_out as u128 * net / (reserve_in as u128 + net);
    require!(out > 0, PoolError::ZeroAmount);
    Ok(out as u64)
}

#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub fee_bps: u16,
    pub bump: u8,
}

#[derive(Accounts)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub mint_a: Box<Account<'info, Mint>>,
    pub mint_b: Box<Account<'info, Mint>>,
    #[account(init, payer = payer, space = 8 + Pool::INIT_SPACE, seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()], bump)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(init, payer = payer, token::mint = mint_a, token::authority = pool, seeds = [VAULT_SEED, pool.key().as_ref(), mint_a.key().as_ref()], bump)]
    pub vault_a: Box<Account<'info, TokenAccount>>,
    #[account(init, payer = payer, token::mint = mint_b, token::authority = pool, seeds = [VAULT_SEED, pool.key().as_ref(), mint_b.key().as_ref()], bump)]
    pub vault_b: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = mint_a, token::authority = payer)]
    pub payer_a: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = mint_b, token::authority = payer)]
    pub payer_b: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Swap<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()], bump = pool.bump, has_one = vault_a, has_one = vault_b)]
    pub pool: Box<Account<'info, Pool>>,
    pub mint_a: Box<Account<'info, Mint>>,
    pub mint_b: Box<Account<'info, Mint>>,
    #[account(mut)]
    pub vault_a: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub vault_b: Box<Account<'info, TokenAccount>>,
    /// The user's associated token accounts. Either is created here when missing, so a
    /// swap is a single instruction for a wallet that has never held the output mint.
    #[account(init_if_needed, payer = user, associated_token::mint = mint_a, associated_token::authority = user)]
    pub user_a: Box<Account<'info, TokenAccount>>,
    #[account(init_if_needed, payer = user, associated_token::mint = mint_b, associated_token::authority = user)]
    pub user_b: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[error_code]
pub enum PoolError {
    #[msg("The fee must be under 10000 basis points")]
    InvalidFee,
    #[msg("The amount is zero, or too small to produce any output")]
    ZeroAmount,
    #[msg("The output is under the minimum")]
    SlippageExceeded,
}
