use crate::error::MakerError;
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

/// Operator moves inventory in or out of a market vault. `mint` must be the
/// market's outcome or collateral mint; the vault is that mint's associated
/// token account owned by the market PDA.
#[derive(Accounts)]
pub struct Liquidity<'info> {
    pub operator: Signer<'info>,
    #[account(has_one = operator @ MakerError::NotOperator)]
    pub maker: Account<'info, Maker>,
    #[account(mut, has_one = maker)]
    pub market: Account<'info, Market>,
    #[account(
        constraint = mint.key() == market.outcome_mint || mint.key() == market.collateral_mint @ MakerError::BadPair
    )]
    pub mint: Box<Account<'info, Mint>>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = market)]
    pub vault: Box<Account<'info, TokenAccount>>,
    /// Source for deposits (must be owned by the operator, enforced by the
    /// token program) or destination for withdrawals (any owner).
    #[account(mut, token::mint = mint)]
    pub counterparty: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

/// Deposits of outcome tokens raise `position`; this is how the maker gets
/// shares to sell. Collateral deposits change nothing but the vault balance.
pub fn deposit(ctx: Context<Liquidity>, amount: u64) -> Result<()> {
    token::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.counterparty.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.operator.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;
    let m = &mut ctx.accounts.market;
    if ctx.accounts.mint.key() == m.outcome_mint {
        m.position = m
            .position
            .checked_add(amount as i128)
            .ok_or(MakerError::MathOverflow)?;
    }
    Ok(())
}

/// Withdrawing outcome tokens lowers `position`; withdrawing collateral
/// changes nothing but the vault balance.
pub fn withdraw(ctx: Context<Liquidity>, amount: u64) -> Result<()> {
    let m = &ctx.accounts.market;
    let maker_key = m.maker;
    let outcome_mint = m.outcome_mint;
    let seeds: &[&[u8]] = &[
        MARKET_SEED,
        maker_key.as_ref(),
        outcome_mint.as_ref(),
        &[m.bump],
    ];
    token::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.vault.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.counterparty.to_account_info(),
                authority: ctx.accounts.market.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;
    let m = &mut ctx.accounts.market;
    if ctx.accounts.mint.key() == m.outcome_mint {
        m.position = m
            .position
            .checked_sub(amount as i128)
            .ok_or(MakerError::MathOverflow)?;
    }
    Ok(())
}
