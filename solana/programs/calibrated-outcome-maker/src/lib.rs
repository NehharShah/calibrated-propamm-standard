//! Calibrated Outcome Maker: a market maker for binary outcome tokens that
//! publishes the calibration of the forecaster behind its quotes, derives a
//! spread floor from it, and never quotes or fills inside that floor.
//!
//! Solana port of the Solidity implementation in `../../evm`. Pricing is in
//! `math`; account layouts in `state`; one module per instruction group.
//! Theory: Shah (2026), "Calibrated Agents as Market Makers", Digital Finance,
//! Digital Object Identifier (DOI) 10.1007/s42521-026-00233-z.

#![allow(unexpected_cfgs)]
#![allow(clippy::result_large_err)]

pub mod error;
pub mod events;
pub mod instructions;
pub mod math;
pub mod state;

use anchor_lang::prelude::*;
use instructions::*;
use state::{CalibrationInput, Quote, QuoteParams, SignedQuote};

declare_id!("HYT6rgCrDK9inEYt5UaVgSPwrChudbgZBsLvnoiBSBnE");

#[program]
pub mod calibrated_outcome_maker {
    use super::*;

    // ---------------------------------------------------------------- admin

    pub fn init_maker(
        ctx: Context<InitMaker>,
        attester: Pubkey,
        quote_signer: Pubkey,
        max_record_age: i64,
        min_sample_size: u32,
    ) -> Result<()> {
        instructions::admin::init_maker(
            ctx,
            attester,
            quote_signer,
            max_record_age,
            min_sample_size,
        )
    }

    pub fn set_market(ctx: Context<SetMarket>, model_id: [u8; 32]) -> Result<()> {
        instructions::admin::set_market(ctx, model_id)
    }

    pub fn set_active(ctx: Context<OperatorMarket>, active: bool) -> Result<()> {
        instructions::admin::set_active(ctx, active)
    }

    pub fn set_quote_params(ctx: Context<OperatorMarket>, params: QuoteParams) -> Result<()> {
        instructions::admin::set_quote_params(ctx, params)
    }

    /// Markets are passed as remaining accounts, one per entry of `params`.
    pub fn set_quote_params_batch<'info>(
        ctx: Context<'info, OperatorBatch<'info>>,
        params: Vec<QuoteParams>,
    ) -> Result<()> {
        instructions::admin::set_quote_params_batch(ctx, params)
    }

    pub fn set_calibration(
        ctx: Context<SetCalibration>,
        model_id: [u8; 32],
        record: CalibrationInput,
    ) -> Result<()> {
        instructions::calibration::set_calibration(ctx, model_id, record)
    }

    pub fn deposit(ctx: Context<Liquidity>, amount: u64) -> Result<()> {
        instructions::liquidity::deposit(ctx, amount)
    }

    pub fn withdraw(ctx: Context<Liquidity>, amount: u64) -> Result<()> {
        instructions::liquidity::withdraw(ctx, amount)
    }

    // ---------------------------------------------------------------- views

    pub fn spread_floor_bps(ctx: Context<View>) -> Result<u64> {
        instructions::views::spread_floor_bps(ctx)
    }

    pub fn quote_bps(ctx: Context<View>) -> Result<Quote> {
        instructions::views::quote_bps(ctx)
    }

    pub fn improves_book(ctx: Context<View>, ref_bid_bps: u64, ref_ask_bps: u64) -> Result<bool> {
        instructions::views::improves_book(ctx, ref_bid_bps, ref_ask_bps)
    }

    pub fn get_amount_out(
        ctx: Context<GetAmountOut>,
        token_in: Pubkey,
        token_out: Pubkey,
        amount_in: u64,
    ) -> Result<u64> {
        instructions::views::get_amount_out(ctx, token_in, token_out, amount_in)
    }

    // ----------------------------------------------------------------- swap

    pub fn swap(
        ctx: Context<Swap>,
        amount_in: u64,
        amount_out_min: u64,
        deadline: i64,
    ) -> Result<u64> {
        instructions::swap::swap(ctx, amount_in, amount_out_min, deadline)
    }

    /// Same as `swap`, priced from `quote`, which the maker's quote signer
    /// authorises by co-signing the transaction.
    pub fn swap_with_quote(
        ctx: Context<SwapWithQuote>,
        quote: SignedQuote,
        amount_in: u64,
        amount_out_min: u64,
        deadline: i64,
    ) -> Result<u64> {
        instructions::swap::swap_with_quote(ctx, quote, amount_in, amount_out_min, deadline)
    }
}
