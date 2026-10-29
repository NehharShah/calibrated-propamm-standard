//! Read-only instructions. Solana has no view calls, so these return their
//! result as instruction return data; clients simulate the transaction and
//! read it, or recompute from account data with `math`.

use crate::instructions::calibration::load_record;
use crate::instructions::swap::{direction, price_fill, quote_inputs};
use crate::math;
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

#[derive(Accounts)]
pub struct View<'info> {
    pub maker: Account<'info, Maker>,
    #[account(has_one = maker)]
    pub market: Account<'info, Market>,
    /// CHECK: calibration record PDA for `market.model_id`, read manually.
    #[account(seeds = [RECORD_SEED, maker.key().as_ref(), market.model_id.as_ref()], bump)]
    pub record: UncheckedAccount<'info>,
}

fn floor_of(ctx: &Context<View>) -> Result<u64> {
    let now = Clock::get()?.unix_timestamp;
    let record = load_record(&ctx.accounts.record.to_account_info())?;
    math::valid_floor(&record, now, ctx.accounts.maker.max_record_age)
}

/// Minimum spread the maker commits to, in bps. Fails under the same
/// conditions as `quote_bps`, so a caller never reads a floor the maker
/// will not honour.
pub fn spread_floor_bps(ctx: Context<View>) -> Result<u64> {
    floor_of(&ctx)
}

/// The current two-sided quote. `ask - bid >= spread_floor_bps` always holds.
pub fn quote_bps(ctx: Context<View>) -> Result<Quote> {
    let floor = floor_of(&ctx)?;
    let (bid_bps, ask_bps) = math::quote_bps(&quote_inputs(&ctx.accounts.market), floor)?;
    Ok(Quote { bid_bps, ask_bps })
}

/// Whether the current quote tightens both sides of a book whose best bid
/// and ask are the arguments (the paper's Theorem 4.6).
pub fn improves_book(ctx: Context<View>, ref_bid_bps: u64, ref_ask_bps: u64) -> Result<bool> {
    let q = quote_bps(ctx)?;
    Ok(math::improves_book(
        q.bid_bps,
        q.ask_bps,
        ref_bid_bps,
        ref_ask_bps,
    ))
}

#[derive(Accounts)]
pub struct GetAmountOut<'info> {
    pub maker: Account<'info, Maker>,
    #[account(has_one = maker)]
    pub market: Account<'info, Market>,
    /// CHECK: calibration record PDA for `market.model_id`, read manually.
    #[account(seeds = [RECORD_SEED, maker.key().as_ref(), market.model_id.as_ref()], bump)]
    pub record: UncheckedAccount<'info>,
    #[account(associated_token::mint = market.outcome_mint, associated_token::authority = market)]
    pub outcome_vault: Box<Account<'info, TokenAccount>>,
    #[account(associated_token::mint = market.collateral_mint, associated_token::authority = market)]
    pub collateral_vault: Box<Account<'info, TokenAccount>>,
}

/// Exact-input quote for a fill of `amount_in` of `token_in`, identical to
/// what `swap` would deliver in the same state.
pub fn get_amount_out(
    ctx: Context<GetAmountOut>,
    token_in: Pubkey,
    token_out: Pubkey,
    amount_in: u64,
) -> Result<u64> {
    let m = &ctx.accounts.market;
    let dir = direction(m, token_in, token_out)?;
    let have_out = if dir.buying_outcome {
        ctx.accounts.outcome_vault.amount
    } else {
        ctx.accounts.collateral_vault.amount
    };
    let (amount_out, _) = price_fill(
        &ctx.accounts.maker,
        m,
        &ctx.accounts.record.to_account_info(),
        dir.buying_outcome,
        amount_in,
        have_out,
    )?;
    Ok(amount_out)
}
