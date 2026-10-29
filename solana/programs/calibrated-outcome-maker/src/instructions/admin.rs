use crate::error::MakerError;
use crate::events::{MarketActiveSet, MarketSet, QuoteParamsSet};
use crate::math::{BPS, MAX_DECIMALS};
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{Mint, Token, TokenAccount};

#[derive(Accounts)]
pub struct InitMaker<'info> {
    #[account(mut)]
    pub operator: Signer<'info>,
    #[account(
        init,
        payer = operator,
        space = 8 + Maker::INIT_SPACE,
        seeds = [MAKER_SEED, operator.key().as_ref()],
        bump
    )]
    pub maker: Account<'info, Maker>,
    pub system_program: Program<'info, System>,
}

pub fn init_maker(
    ctx: Context<InitMaker>,
    attester: Pubkey,
    quote_signer: Pubkey,
    max_record_age: i64,
    min_sample_size: u32,
) -> Result<()> {
    require!(attester != Pubkey::default(), MakerError::ZeroAddress);
    require!(max_record_age >= 0, MakerError::BadRecordTime);
    let maker = &mut ctx.accounts.maker;
    maker.operator = ctx.accounts.operator.key();
    maker.attester = attester;
    maker.quote_signer = quote_signer;
    maker.max_record_age = max_record_age;
    maker.min_sample_size = min_sample_size;
    maker.bump = ctx.bumps.maker;
    Ok(())
}

/// Configure a market once. A mint may be an outcome token in one market or a
/// collateral in any number of markets, never both. Repeating the call for the
/// same outcome mint fails because the market account already exists.
#[derive(Accounts)]
pub struct SetMarket<'info> {
    #[account(mut)]
    pub operator: Signer<'info>,
    #[account(has_one = operator @ MakerError::NotOperator)]
    pub maker: Account<'info, Maker>,
    pub outcome_mint: Box<Account<'info, Mint>>,
    pub collateral_mint: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = operator,
        space = 8 + Market::INIT_SPACE,
        seeds = [MARKET_SEED, maker.key().as_ref(), outcome_mint.key().as_ref()],
        bump
    )]
    pub market: Account<'info, Market>,
    #[account(
        init_if_needed,
        payer = operator,
        space = 8 + CollateralMarker::INIT_SPACE,
        seeds = [COLLATERAL_SEED, maker.key().as_ref(), collateral_mint.key().as_ref()],
        bump
    )]
    pub collateral_marker: Account<'info, CollateralMarker>,
    /// CHECK: PDA that would mark the outcome mint as a collateral. It must not
    /// exist; only its address is verified here and its emptiness in the handler.
    #[account(seeds = [COLLATERAL_SEED, maker.key().as_ref(), outcome_mint.key().as_ref()], bump)]
    pub outcome_as_collateral: UncheckedAccount<'info>,
    /// CHECK: PDA of the market that would make the collateral mint an outcome
    /// token. It must not exist; address verified here, emptiness in the handler.
    #[account(seeds = [MARKET_SEED, maker.key().as_ref(), collateral_mint.key().as_ref()], bump)]
    pub collateral_as_outcome: UncheckedAccount<'info>,
    #[account(
        init,
        payer = operator,
        associated_token::mint = outcome_mint,
        associated_token::authority = market
    )]
    pub outcome_vault: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = operator,
        associated_token::mint = collateral_mint,
        associated_token::authority = market
    )]
    pub collateral_vault: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn set_market(ctx: Context<SetMarket>, model_id: [u8; 32]) -> Result<()> {
    let outcome = &ctx.accounts.outcome_mint;
    let collateral = &ctx.accounts.collateral_mint;
    require!(
        outcome.key() != collateral.key(),
        MakerError::TokenRoleCollision
    );
    require!(
        ctx.accounts.outcome_as_collateral.data_is_empty(),
        MakerError::TokenRoleCollision
    );
    require!(
        ctx.accounts.collateral_as_outcome.data_is_empty(),
        MakerError::TokenRoleCollision
    );
    require!(outcome.decimals <= MAX_DECIMALS, MakerError::BadDecimals);
    require!(collateral.decimals <= MAX_DECIMALS, MakerError::BadDecimals);

    let maker_key = ctx.accounts.maker.key();
    let m = &mut ctx.accounts.market;
    m.maker = maker_key;
    m.outcome_mint = outcome.key();
    m.collateral_mint = collateral.key();
    m.model_id = model_id;
    m.forecast_bps = 0;
    m.operator_spread_bps = 0;
    m.skew_bps_per_unit = 0;
    m.outcome_decimals = outcome.decimals;
    m.collateral_decimals = collateral.decimals;
    m.active = true;
    m.inventory_limit = 0;
    m.position = 0;
    m.bump = ctx.bumps.market;

    let marker = &mut ctx.accounts.collateral_marker;
    marker.maker = maker_key;
    marker.mint = collateral.key();
    marker.bump = ctx.bumps.collateral_marker;

    emit!(MarketSet {
        market: m.key(),
        outcome_mint: outcome.key(),
        collateral_mint: collateral.key(),
        model_id,
    });
    Ok(())
}

#[derive(Accounts)]
pub struct OperatorMarket<'info> {
    pub operator: Signer<'info>,
    #[account(has_one = operator @ MakerError::NotOperator)]
    pub maker: Account<'info, Maker>,
    #[account(mut, has_one = maker)]
    pub market: Account<'info, Market>,
}

/// Pause or resume quoting. Inactive markets fail fast with `BadPair`.
pub fn set_active(ctx: Context<OperatorMarket>, active: bool) -> Result<()> {
    let m = &mut ctx.accounts.market;
    m.active = active;
    emit!(MarketActiveSet {
        market: m.key(),
        active
    });
    Ok(())
}

pub fn set_quote_params(ctx: Context<OperatorMarket>, params: QuoteParams) -> Result<()> {
    apply_quote_params(&mut ctx.accounts.market, &params)
}

fn apply_quote_params(m: &mut Account<Market>, p: &QuoteParams) -> Result<()> {
    require!(
        p.forecast_bps as u64 <= BPS && p.operator_spread_bps as u64 <= BPS,
        MakerError::BadBps
    );
    m.forecast_bps = p.forecast_bps;
    m.operator_spread_bps = p.operator_spread_bps;
    m.skew_bps_per_unit = p.skew_bps_per_unit;
    m.inventory_limit = p.inventory_limit;
    emit!(QuoteParamsSet {
        market: m.key(),
        forecast_bps: p.forecast_bps,
        operator_spread_bps: p.operator_spread_bps,
        skew_bps_per_unit: p.skew_bps_per_unit,
        inventory_limit: p.inventory_limit,
    });
    Ok(())
}

#[derive(Accounts)]
pub struct OperatorBatch<'info> {
    pub operator: Signer<'info>,
    #[account(has_one = operator @ MakerError::NotOperator)]
    pub maker: Account<'info, Maker>,
}

/// Update many markets in one transaction. The markets are passed as
/// remaining accounts, one per entry of `params`, in the same order. For a
/// long tail of thin markets this is the cost that scales, so it is one
/// instruction, not many.
pub fn set_quote_params_batch<'info>(
    ctx: Context<'info, OperatorBatch<'info>>,
    params: Vec<QuoteParams>,
) -> Result<()> {
    require!(
        ctx.remaining_accounts.len() == params.len(),
        MakerError::LengthMismatch
    );
    let maker_key = ctx.accounts.maker.key();
    for (info, p) in ctx.remaining_accounts.iter().zip(params.iter()) {
        require!(info.is_writable, MakerError::NotWritable);
        let mut market = Account::<Market>::try_from(info)?;
        require_keys_eq!(market.maker, maker_key, MakerError::UnknownMarket);
        apply_quote_params(&mut market, p)?;
        market.exit(ctx.program_id)?;
    }
    Ok(())
}
