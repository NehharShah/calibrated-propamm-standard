use crate::error::MakerError;
use crate::events::CalibrationUpdated;
use crate::math::BPS;
use crate::state::*;
use anchor_lang::prelude::*;

#[derive(Accounts)]
#[instruction(model_id: [u8; 32])]
pub struct SetCalibration<'info> {
    #[account(mut)]
    pub attester: Signer<'info>,
    #[account(has_one = attester @ MakerError::NotAttester)]
    pub maker: Account<'info, Maker>,
    #[account(
        init_if_needed,
        payer = attester,
        space = 8 + CalibrationRecord::INIT_SPACE,
        seeds = [RECORD_SEED, maker.key().as_ref(), model_id.as_ref()],
        bump
    )]
    pub record: Account<'info, CalibrationRecord>,
    pub system_program: Program<'info, System>,
}

/// Publish or replace the calibration record for a forecaster. Enforces the
/// paper's methodological requirements onchain: the sample must be large
/// enough to mean something, every question in it must have resolved before
/// the record was produced and, when a training cutoff is known, after that
/// cutoff (otherwise the measured error is partly recall, not forecasting).
/// A record dated in the future would defeat the age limit, so it is refused.
pub fn set_calibration(
    ctx: Context<SetCalibration>,
    model_id: [u8; 32],
    r: CalibrationInput,
) -> Result<()> {
    let maker = &ctx.accounts.maker;
    let now = Clock::get()?.unix_timestamp;

    require!(
        r.alpha_bps as u64 <= BPS && r.phi_bps as u64 <= BPS,
        MakerError::BadBps
    );
    require!(
        r.sample_size >= maker.min_sample_size && r.sample_size != 0,
        MakerError::SampleTooSmall
    );
    require!(
        r.measured_at <= now && r.earliest_resolution <= r.measured_at,
        MakerError::BadRecordTime
    );
    require!(
        r.training_cutoff == 0 || r.earliest_resolution > r.training_cutoff,
        MakerError::SampleBeforeCutoff
    );
    require!(r.dataset_uri.len() <= MAX_URI_LEN, MakerError::UriTooLong);

    let rec = &mut ctx.accounts.record;
    rec.maker = maker.key();
    rec.model_id = model_id;
    rec.alpha_bps = r.alpha_bps;
    rec.phi_bps = r.phi_bps;
    rec.sample_size = r.sample_size;
    rec.excluded = r.excluded;
    rec.measured_at = r.measured_at;
    rec.earliest_resolution = r.earliest_resolution;
    rec.training_cutoff = r.training_cutoff;
    rec.dataset_hash = r.dataset_hash;
    rec.dataset_uri = r.dataset_uri;
    rec.bump = ctx.bumps.record;

    emit!(CalibrationUpdated {
        maker: maker.key(),
        model_id,
        alpha_bps: rec.alpha_bps,
        phi_bps: rec.phi_bps,
        sample_size: rec.sample_size,
        excluded: rec.excluded,
        dataset_hash: rec.dataset_hash,
    });
    Ok(())
}

/// Reads the record behind a market without requiring it to exist, so that a
/// missing record reports `NoCalibration` rather than a deserialization error.
pub fn load_record(info: &AccountInfo) -> Result<CalibrationRecord> {
    require!(!info.data_is_empty(), MakerError::NoCalibration);
    require_keys_eq!(*info.owner, crate::ID, MakerError::NoCalibration);
    let data = info.try_borrow_data()?;
    let mut slice: &[u8] = &data;
    CalibrationRecord::try_deserialize(&mut slice)
}
