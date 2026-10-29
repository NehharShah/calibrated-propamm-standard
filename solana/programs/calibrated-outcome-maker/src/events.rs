use anchor_lang::prelude::*;

#[event]
pub struct MarketSet {
    pub market: Pubkey,
    pub outcome_mint: Pubkey,
    pub collateral_mint: Pubkey,
    pub model_id: [u8; 32],
}

#[event]
pub struct MarketActiveSet {
    pub market: Pubkey,
    pub active: bool,
}

#[event]
pub struct QuoteParamsSet {
    pub market: Pubkey,
    pub forecast_bps: u16,
    pub operator_spread_bps: u16,
    pub skew_bps_per_unit: u16,
    pub inventory_limit: u64,
}

#[event]
pub struct CalibrationUpdated {
    pub maker: Pubkey,
    pub model_id: [u8; 32],
    pub alpha_bps: u16,
    pub phi_bps: u16,
    pub sample_size: u32,
    pub excluded: u32,
    pub dataset_hash: [u8; 32],
}

#[event]
pub struct Swap {
    pub market: Pubkey,
    pub taker: Pubkey,
    pub recipient: Pubkey,
    pub token_in: Pubkey,
    pub token_out: Pubkey,
    pub amount_in: u64,
    pub amount_out: u64,
}
