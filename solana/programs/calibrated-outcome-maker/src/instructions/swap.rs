use crate::error::MakerError;
use crate::events::Swap as SwapEvent;
use crate::instructions::calibration::load_record;
use crate::math::{self, QuoteInputs, BPS};
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

/// Which side of the market a fill is on, derived from the mints involved.
pub struct Direction {
    pub buying_outcome: bool,
}

/// Classifies a (token in, token out) pair against a market. Inactive markets
/// and unrelated mints fail fast with `BadPair`.
pub fn direction(m: &Market, token_in: Pubkey, token_out: Pubkey) -> Result<Direction> {
    require!(m.active, MakerError::BadPair);
    if token_in == m.collateral_mint && token_out == m.outcome_mint {
        Ok(Direction {
            buying_outcome: true,
        })
    } else if token_in == m.outcome_mint && token_out == m.collateral_mint {
        Ok(Direction {
            buying_outcome: false,
        })
    } else {
        err!(MakerError::BadPair)
    }
}

pub fn quote_inputs(m: &Market) -> QuoteInputs {
    QuoteInputs {
        forecast_bps: m.forecast_bps,
        operator_spread_bps: m.operator_spread_bps,
        skew_bps_per_unit: m.skew_bps_per_unit,
        outcome_decimals: m.outcome_decimals,
        inventory_limit: m.inventory_limit,
        position: m.position,
    }
}

/// The part of pricing that does not depend on where the quote came from:
/// exact-input arithmetic, inventory limit, vault balance.
pub fn fill_at(
    m: &Market,
    buying_outcome: bool,
    amount_in: u64,
    bid: u64,
    ask: u64,
    have_out: u64,
) -> Result<(u64, i128)> {
    let (amount_out, would_be) = math::price(
        buying_outcome,
        amount_in,
        bid,
        ask,
        m.outcome_decimals,
        m.collateral_decimals,
        m.position,
        m.inventory_limit,
    )?;
    require!(amount_out <= have_out, MakerError::InsufficientInventory);
    Ok((amount_out, would_be))
}

/// Stored-parameter pricing. Shared by `get_amount_out` and `swap` so the two
/// cannot disagree.
pub fn price_fill(
    maker: &Maker,
    m: &Market,
    record_info: &AccountInfo,
    buying_outcome: bool,
    amount_in: u64,
    have_out: u64,
) -> Result<(u64, i128)> {
    require!(amount_in != 0, MakerError::ZeroAmount);
    let now = Clock::get()?.unix_timestamp;
    let record = load_record(record_info)?;
    let floor = math::valid_floor(&record, now, maker.max_record_age)?;
    let (bid, ask) = math::quote_bps(&quote_inputs(m), floor)?;
    fill_at(m, buying_outcome, amount_in, bid, ask, have_out)
}

/// Signed-quote pricing: the engine's bid and ask are accepted only if the
/// spread is at least the floor the record implies, and the fill is held to
/// the quote's size caps on top of the inventory limit.
pub fn price_signed_fill(
    maker: &Maker,
    m: &Market,
    record_info: &AccountInfo,
    buying_outcome: bool,
    amount_in: u64,
    have_out: u64,
    q: &SignedQuote,
) -> Result<(u64, i128)> {
    require!(amount_in != 0, MakerError::ZeroAmount);
    let now = Clock::get()?.unix_timestamp;
    require!(now <= q.expiry, MakerError::QuoteExpired);
    let record = load_record(record_info)?;
    let floor = math::valid_floor(&record, now, maker.max_record_age)?;
    let (bid, ask) = (q.bid_bps as u64, q.ask_bps as u64);
    require!(
        ask <= BPS && ask >= bid && ask - bid >= floor,
        MakerError::QuoteInsideFloor
    );
    let (amount_out, would_be) = fill_at(m, buying_outcome, amount_in, bid, ask, have_out)?;
    if buying_outcome {
        require!(
            amount_out <= q.max_outcome_out,
            MakerError::QuoteSizeExceeded
        );
    } else {
        require!(amount_in <= q.max_outcome_in, MakerError::QuoteSizeExceeded);
    }
    Ok((amount_out, would_be))
}

#[derive(Accounts)]
pub struct Swap<'info> {
    pub taker: Signer<'info>,
    pub maker: Account<'info, Maker>,
    #[account(mut, has_one = maker)]
    pub market: Account<'info, Market>,
    /// CHECK: the calibration record PDA for `market.model_id`. Read manually
    /// so that a missing record reports `NoCalibration`.
    #[account(seeds = [RECORD_SEED, maker.key().as_ref(), market.model_id.as_ref()], bump)]
    pub record: UncheckedAccount<'info>,
    #[account(address = market.outcome_mint)]
    pub outcome_mint: Box<Account<'info, Mint>>,
    #[account(address = market.collateral_mint)]
    pub collateral_mint: Box<Account<'info, Mint>>,
    #[account(mut, associated_token::mint = outcome_mint, associated_token::authority = market)]
    pub outcome_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = collateral_mint, associated_token::authority = market)]
    pub collateral_vault: Box<Account<'info, TokenAccount>>,
    /// The taker's source account. Its mint decides the direction of the fill.
    #[account(mut)]
    pub taker_in: Box<Account<'info, TokenAccount>>,
    /// Recipient of the output. Any token account of the output mint.
    #[account(mut)]
    pub taker_out: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

/// `swap` plus the quoting engine's key as a transaction co-signer.
#[derive(Accounts)]
pub struct SwapWithQuote<'info> {
    pub base: Swap<'info>,
    /// The maker's quote signer. Its signature on this transaction is the
    /// authorisation for the quote in the instruction data. No key can sign
    /// as `Pubkey::default()`, so a maker initialised with it has this path
    /// disabled.
    #[account(constraint = quote_signer.key() == base.maker.quote_signer @ MakerError::BadSigner)]
    pub quote_signer: Signer<'info>,
}

/// Exact-input swap priced from stored parameters.
pub fn swap(ctx: Context<Swap>, amount_in: u64, amount_out_min: u64, deadline: i64) -> Result<u64> {
    let now = Clock::get()?.unix_timestamp;
    require!(now <= deadline, MakerError::Expired);
    let acc = &mut *ctx.accounts;
    let dir = direction(&acc.market, acc.taker_in.mint, acc.taker_out.mint)?;
    let have_out = if dir.buying_outcome {
        acc.outcome_vault.amount
    } else {
        acc.collateral_vault.amount
    };
    let (amount_out, would_be) = price_fill(
        &acc.maker,
        &acc.market,
        &acc.record.to_account_info(),
        dir.buying_outcome,
        amount_in,
        have_out,
    )?;
    require!(amount_out >= amount_out_min, MakerError::Slippage);
    settle(acc, dir.buying_outcome, amount_in, amount_out, would_be)?;
    Ok(amount_out)
}

/// Exact-input swap priced from a quote the engine co-signed. Same floor,
/// inventory and balance checks as `swap`; the engine only chooses where
/// inside the allowed region the price sits.
pub fn swap_with_quote(
    ctx: Context<SwapWithQuote>,
    quote: SignedQuote,
    amount_in: u64,
    amount_out_min: u64,
    deadline: i64,
) -> Result<u64> {
    let now = Clock::get()?.unix_timestamp;
    require!(now <= deadline, MakerError::Expired);
    let acc = &mut ctx.accounts.base;
    let dir = direction(&acc.market, acc.taker_in.mint, acc.taker_out.mint)?;
    let have_out = if dir.buying_outcome {
        acc.outcome_vault.amount
    } else {
        acc.collateral_vault.amount
    };
    let (amount_out, would_be) = price_signed_fill(
        &acc.maker,
        &acc.market,
        &acc.record.to_account_info(),
        dir.buying_outcome,
        amount_in,
        have_out,
        &quote,
    )?;
    require!(amount_out >= amount_out_min, MakerError::Slippage);
    settle(acc, dir.buying_outcome, amount_in, amount_out, would_be)?;
    Ok(amount_out)
}

/// Writes the position, moves the tokens, emits the event. Effects before
/// interactions, as in the Solidity version; Solana forbids re-entering this
/// program from the token program anyway.
fn settle<'info>(
    acc: &mut Swap<'info>,
    buying_outcome: bool,
    amount_in: u64,
    amount_out: u64,
    would_be: i128,
) -> Result<()> {
    let (vault_in, vault_out, mint_in, mint_out) = if buying_outcome {
        (
            &acc.collateral_vault,
            &acc.outcome_vault,
            &acc.collateral_mint,
            &acc.outcome_mint,
        )
    } else {
        (
            &acc.outcome_vault,
            &acc.collateral_vault,
            &acc.outcome_mint,
            &acc.collateral_mint,
        )
    };

    let market_key = acc.market.key();
    let maker_key = acc.market.maker;
    let outcome_mint_key = acc.market.outcome_mint;
    let bump = acc.market.bump;
    acc.market.position = would_be;

    token::transfer_checked(
        CpiContext::new(
            acc.token_program.key(),
            TransferChecked {
                from: acc.taker_in.to_account_info(),
                mint: mint_in.to_account_info(),
                to: vault_in.to_account_info(),
                authority: acc.taker.to_account_info(),
            },
        ),
        amount_in,
        mint_in.decimals,
    )?;

    let seeds: &[&[u8]] = &[
        MARKET_SEED,
        maker_key.as_ref(),
        outcome_mint_key.as_ref(),
        &[bump],
    ];
    token::transfer_checked(
        CpiContext::new_with_signer(
            acc.token_program.key(),
            TransferChecked {
                from: vault_out.to_account_info(),
                mint: mint_out.to_account_info(),
                to: acc.taker_out.to_account_info(),
                authority: acc.market.to_account_info(),
            },
            &[seeds],
        ),
        amount_out,
        mint_out.decimals,
    )?;

    emit!(SwapEvent {
        market: market_key,
        taker: acc.taker.key(),
        recipient: acc.taker_out.key(),
        token_in: acc.taker_in.mint,
        token_out: acc.taker_out.mint,
        amount_in,
        amount_out,
    });
    Ok(())
}
