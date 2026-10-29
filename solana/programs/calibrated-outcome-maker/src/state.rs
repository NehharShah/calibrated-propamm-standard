use anchor_lang::prelude::*;

pub const MAKER_SEED: &[u8] = b"maker";
pub const MARKET_SEED: &[u8] = b"market";
pub const RECORD_SEED: &[u8] = b"record";
pub const COLLATERAL_SEED: &[u8] = b"collateral";

/// Longest dataset URI a record may carry. The URI is never read on the
/// quote or swap path, so this bounds rent, not compute.
pub const MAX_URI_LEN: usize = 200;

/// One maker: a role pair and the record rules it enforces. Program derived
/// address (PDA) seeded by the operator, so one maker per operator key.
#[account]
#[derive(InitSpace)]
pub struct Maker {
    pub operator: Pubkey,
    pub attester: Pubkey,
    /// Key the offchain quoting engine co-signs `swap_with_quote` with.
    /// `Pubkey::default()` disables the signed path.
    pub quote_signer: Pubkey,
    /// Seconds a calibration record stays valid after `measured_at`.
    pub max_record_age: i64,
    pub min_sample_size: u32,
    pub bump: u8,
}

/// One market: the pair (collateral mint, outcome mint). PDA seeded by the
/// maker and the outcome mint, so an outcome mint belongs to one market.
/// Token vaults are the associated token accounts of this PDA.
#[account]
#[derive(InitSpace)]
pub struct Market {
    pub maker: Pubkey,
    pub outcome_mint: Pubkey,
    pub collateral_mint: Pubkey,
    pub model_id: [u8; 32],
    /// Forecaster's probability estimate, in basis points (bps).
    pub forecast_bps: u16,
    /// Spread the operator would like; the floor may override it.
    pub operator_spread_bps: u16,
    /// Bps of mid shift per whole outcome token held (beta).
    pub skew_bps_per_unit: u16,
    pub outcome_decimals: u8,
    pub collateral_decimals: u8,
    pub active: bool,
    /// Q_max in outcome token base units.
    pub inventory_limit: u64,
    /// Net outcome tokens from fills and operator deposits and withdrawals,
    /// in base units. Positive means the maker is long the outcome.
    pub position: i128,
    pub bump: u8,
}

/// Calibration record for one forecaster. PDA seeded by the maker and the
/// model id. Field meanings follow the Solidity interface in `../../evm`.
#[account]
#[derive(InitSpace)]
pub struct CalibrationRecord {
    pub maker: Pubkey,
    pub model_id: [u8; 32],
    /// Mean absolute error between forecast and realized outcome, in bps.
    pub alpha_bps: u16,
    /// Informed fraction of order flow the maker assumes, in bps.
    pub phi_bps: u16,
    /// Number of resolved forecasts the error is measured on.
    pub sample_size: u32,
    /// Forecasts excluded from the sample, disclosed so `sample_size` cannot
    /// silently shrink to the convenient subset.
    pub excluded: u32,
    /// Unix time the record was produced. Not in the future, not before
    /// `earliest_resolution`.
    pub measured_at: i64,
    /// Unix time of the earliest question resolution in the sample.
    pub earliest_resolution: i64,
    /// Unix time of the forecaster's knowledge cutoff, or 0 if none.
    pub training_cutoff: i64,
    /// Hash of the canonical per-forecast record set.
    pub dataset_hash: [u8; 32],
    #[max_len(200)]
    pub dataset_uri: String,
    pub bump: u8,
}

/// Marks a mint as collateral for this maker, so it can never become an
/// outcome mint. PDA seeded by the maker and the mint.
#[account]
#[derive(InitSpace)]
pub struct CollateralMarker {
    pub maker: Pubkey,
    pub mint: Pubkey,
    pub bump: u8,
}

/// Instruction argument for `set_calibration`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CalibrationInput {
    pub alpha_bps: u16,
    pub phi_bps: u16,
    pub sample_size: u32,
    pub excluded: u32,
    pub measured_at: i64,
    pub earliest_resolution: i64,
    pub training_cutoff: i64,
    pub dataset_hash: [u8; 32],
    pub dataset_uri: String,
}

/// Stored-parameter quote inputs, used one market at a time or in a batch.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct QuoteParams {
    pub forecast_bps: u16,
    pub operator_spread_bps: u16,
    pub skew_bps_per_unit: u16,
    pub inventory_limit: u64,
}

/// A quote produced offchain. The quoting engine authorises it by co-signing
/// the transaction with `Maker::quote_signer`, so the quote is bound to this
/// exact transaction and cannot be replayed or altered.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct SignedQuote {
    /// Price at which the maker buys the outcome token, in bps.
    pub bid_bps: u16,
    /// Price at which the maker sells it. `ask - bid` must be at least the floor.
    pub ask_bps: u16,
    /// Most outcome tokens the maker will sell on this quote, base units.
    pub max_outcome_out: u64,
    /// Most outcome tokens the maker will buy on this quote, base units.
    pub max_outcome_in: u64,
    /// Unix time after which the quote is refused.
    pub expiry: i64,
}

/// Return value of the `quote_bps` view.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub bid_bps: u64,
    pub ask_bps: u64,
}
