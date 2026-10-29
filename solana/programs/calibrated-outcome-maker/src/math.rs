//! Pricing arithmetic, kept free of account plumbing so that clients can
//! reproduce a quote offchain from account data and so that the invariants
//! can be property-tested on the host without a validator.
//!
//! Pricing follows Shah (2026), Digital Finance, Digital Object Identifier
//! (DOI) 10.1007/s42521-026-00233-z:
//!   floor  = ceil(2 * phi * alpha)              (Corollary 4.7 profitability)
//!   spread = max(operator_spread, floor)
//!   mid    = forecast - beta * position          (Section 6.5 inventory skew)
//!   bid    = clamp(mid - floor(spread / 2), 0, 10_000 - spread)
//!   ask    = bid + spread
//! All probabilities and prices are in basis points (bps), 0 to 10_000.

use crate::error::MakerError;
use crate::state::CalibrationRecord;
use anchor_lang::prelude::*;

pub const BPS: u64 = 10_000;

/// Largest mint decimals accepted. With 18 decimals every intermediate
/// product below fits in u128 for any u64 amount; the checked arithmetic
/// still guards the edges.
pub const MAX_DECIMALS: u8 = 18;

#[inline]
pub fn pow10(decimals: u8) -> u128 {
    10u128.pow(decimals as u32)
}

/// The inputs to a quote, as read from a `Market` account.
#[derive(Clone, Copy, Debug)]
pub struct QuoteInputs {
    pub forecast_bps: u16,
    pub operator_spread_bps: u16,
    pub skew_bps_per_unit: u16,
    pub outcome_decimals: u8,
    pub inventory_limit: u64,
    pub position: i128,
}

/// `ceil(2 * phi * alpha / BPS)`. Errors when the floor is wider than the
/// whole probability range, because such a floor cannot be honoured and the
/// maker must refuse to quote rather than quote inside it.
pub fn spread_floor_bps(phi_bps: u16, alpha_bps: u16) -> Result<u64> {
    let num = 2 * phi_bps as u64 * alpha_bps as u64;
    let floor = num.div_ceil(BPS);
    require!(floor <= BPS, MakerError::FloorOutOfRange);
    Ok(floor)
}

/// Validates a record against the clock and returns its floor.
pub fn valid_floor(record: &CalibrationRecord, now: i64, max_record_age: i64) -> Result<u64> {
    require!(record.sample_size != 0, MakerError::NoCalibration);
    let expires = record
        .measured_at
        .checked_add(max_record_age)
        .ok_or(MakerError::MathOverflow)?;
    require!(now <= expires, MakerError::CalibrationStale);
    spread_floor_bps(record.phi_bps, record.alpha_bps)
}

/// Two-sided quote in bps. Errors when the skew excursion across the full
/// inventory limit exceeds the spread: a taker who sells the full limit and
/// buys it back would otherwise profit by (skew * limit - spread) per unit.
pub fn quote_bps(q: &QuoteInputs, floor: u64) -> Result<(u64, u64)> {
    let spread = (q.operator_spread_bps as u64).max(floor);
    let unit = pow10(q.outcome_decimals);
    let skew = q.skew_bps_per_unit as u128;

    let max_skew = skew * q.inventory_limit as u128 / unit;
    require!(max_skew <= spread as u128, MakerError::SkewExceedsSpread);

    // Inventory skew: shift mid against the side we hold. Signed division
    // truncates toward zero, as in the Solidity version.
    let shift = (skew as i128)
        .checked_mul(q.position)
        .ok_or(MakerError::MathOverflow)?
        / unit as i128;
    let mid = q.forecast_bps as i128 - shift;

    let lo = mid - (spread / 2) as i128;
    let max_lo = BPS as i128 - spread as i128; // >= 0 because spread <= BPS
    let lo = lo.clamp(0, max_lo);
    let bid = lo as u64;
    Ok((bid, bid + spread))
}

/// Exact-input pricing. Returns the amount out and the position the fill
/// would leave. The caller checks vault balance separately.
#[allow(clippy::too_many_arguments)]
pub fn price(
    buying_outcome: bool,
    amount_in: u64,
    bid: u64,
    ask: u64,
    outcome_decimals: u8,
    collateral_decimals: u8,
    position: i128,
    inventory_limit: u64,
) -> Result<(u64, i128)> {
    require!(amount_in != 0, MakerError::ZeroAmount);
    let out_unit = pow10(outcome_decimals);
    let col_unit = pow10(collateral_decimals);
    let amount_in = amount_in as u128;

    let (amount_out, would_be) = if buying_outcome {
        require!(ask != 0, MakerError::ZeroPrice);
        // outcome tokens = collateral / (ask / BPS), with decimal conversion
        let num = amount_in
            .checked_mul(BPS as u128)
            .and_then(|v| v.checked_mul(out_unit))
            .ok_or(MakerError::MathOverflow)?;
        let den = (ask as u128)
            .checked_mul(col_unit)
            .ok_or(MakerError::MathOverflow)?;
        let out = num / den;
        let would_be = position
            .checked_sub(out as i128)
            .ok_or(MakerError::MathOverflow)?;
        (out, would_be)
    } else {
        let num = amount_in
            .checked_mul(bid as u128)
            .and_then(|v| v.checked_mul(col_unit))
            .ok_or(MakerError::MathOverflow)?;
        let den = (BPS as u128)
            .checked_mul(out_unit)
            .ok_or(MakerError::MathOverflow)?;
        let out = num / den;
        let would_be = position
            .checked_add(amount_in as i128)
            .ok_or(MakerError::MathOverflow)?;
        (out, would_be)
    };

    require!(
        would_be.unsigned_abs() <= inventory_limit as u128,
        MakerError::InventoryLimit
    );
    require!(amount_out != 0, MakerError::ZeroOutput);
    let amount_out = u64::try_from(amount_out).map_err(|_| MakerError::MathOverflow)?;
    Ok((amount_out, would_be))
}

/// Two-sided improvement test of the paper's Theorem 4.6: strict on both sides.
pub fn improves_book(bid: u64, ask: u64, ref_bid: u64, ref_ask: u64) -> bool {
    bid > ref_bid && ask < ref_ask
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const DEC: u8 = 6;
    const UNIT: u128 = 1_000_000;
    const Q_MAX: u64 = 5_000_000; // five whole tokens

    fn inputs(forecast: u16, op_spread: u16, skew: u16, position: i128) -> QuoteInputs {
        QuoteInputs {
            forecast_bps: forecast,
            operator_spread_bps: op_spread,
            skew_bps_per_unit: skew,
            outcome_decimals: DEC,
            inventory_limit: Q_MAX,
            position,
        }
    }

    fn code(e: Error) -> u32 {
        match e {
            Error::AnchorError(a) => a.error_code_number,
            other => panic!("unexpected error {other:?}"),
        }
    }

    fn is(e: Error, want: MakerError) -> bool {
        code(e) == want as u32 + anchor_lang::error::ERROR_CODE_OFFSET
    }

    // Calibration examples from the paper as of June 2026: alpha 0.149
    // (best model, 15 question set) and 0.354 (60 question set), phi 0.15.
    #[test]
    fn floor_from_paper_numbers() {
        assert_eq!(spread_floor_bps(1500, 1490).unwrap(), 447);
        assert_eq!(spread_floor_bps(1500, 3540).unwrap(), 1062);
    }

    #[test]
    fn floor_rounds_up() {
        assert_eq!(spread_floor_bps(1500, 1).unwrap(), 1);
        assert_eq!(spread_floor_bps(0, 1).unwrap(), 0);
    }

    #[test]
    fn floor_out_of_range_refused() {
        // 2 * 0.75 * 0.70 = 1.05 > 1
        assert!(is(
            spread_floor_bps(7500, 7000).unwrap_err(),
            MakerError::FloorOutOfRange
        ));
        // exactly 1.0 is allowed
        assert_eq!(spread_floor_bps(5000, 10_000).unwrap(), 10_000);
    }

    #[test]
    fn quote_basic_and_odd_spread() {
        let (bid, ask) = quote_bps(&inputs(6000, 0, 0, 0), 447).unwrap();
        assert_eq!((bid, ask), (5777, 6224)); // ask one bp further from mid
    }

    #[test]
    fn quote_clamps() {
        let (bid, ask) = quote_bps(&inputs(9990, 0, 0, 0), 447).unwrap();
        assert_eq!((bid, ask), (9553, 10_000));
        let (bid, ask) = quote_bps(&inputs(10, 0, 0, 0), 447).unwrap();
        assert_eq!((bid, ask), (0, 447));
        let (bid, ask) = quote_bps(&inputs(4000, 10_000, 0, 0), 447).unwrap();
        assert_eq!((bid, ask), (0, 10_000));
    }

    #[test]
    fn skew_moves_mid_against_held_side() {
        // 89 bps per token across five tokens is 445 <= 447, so allowed.
        let (bid0, _) = quote_bps(&inputs(6000, 0, 89, 0), 447).unwrap();
        let (bid_long, ask_long) = quote_bps(&inputs(6000, 0, 89, 2 * UNIT as i128), 447).unwrap();
        let (bid_short, _) = quote_bps(&inputs(6000, 0, 89, -(2 * UNIT as i128)), 447).unwrap();
        assert_eq!(bid_long, bid0 - 178);
        assert_eq!(ask_long - bid_long, 447);
        assert_eq!(bid_short, bid0 + 178);
    }

    #[test]
    fn skew_exceeding_spread_refused() {
        // 500 bps per token across five tokens is 2500 > 447
        assert!(is(
            quote_bps(&inputs(6000, 0, 500, 0), 447).unwrap_err(),
            MakerError::SkewExceedsSpread
        ));
    }

    #[test]
    fn price_both_directions() {
        let (bid, ask) = (5777u64, 6224u64);
        // buy one unit of collateral worth of outcome
        let (out, would_be) = price(true, UNIT as u64, bid, ask, DEC, DEC, 0, Q_MAX).unwrap();
        assert_eq!(out as u128, UNIT * 10_000 / 6224);
        assert_eq!(would_be, -(out as i128));
        // sell one outcome token
        let (out, would_be) = price(false, UNIT as u64, bid, ask, DEC, DEC, 0, Q_MAX).unwrap();
        assert_eq!(out as u128, UNIT * 5777 / 10_000);
        assert_eq!(would_be, UNIT as i128);
    }

    #[test]
    fn price_rejections() {
        assert!(is(
            price(true, 0, 5777, 6224, DEC, DEC, 0, Q_MAX).unwrap_err(),
            MakerError::ZeroAmount
        ));
        assert!(is(
            price(true, 1, 0, 0, DEC, DEC, 0, Q_MAX).unwrap_err(),
            MakerError::ZeroPrice
        ));
        // one base unit of outcome sold at bid 5777 bps is worth less than one base unit of collateral
        assert!(is(
            price(false, 1, 5777, 6224, DEC, DEC, 0, Q_MAX).unwrap_err(),
            MakerError::ZeroOutput
        ));
        // selling one base unit more than the limit allows
        assert!(is(
            price(false, Q_MAX + 1, 5777, 6224, DEC, DEC, 0, Q_MAX).unwrap_err(),
            MakerError::InventoryLimit
        ));
        // the limit check runs before the zero output check
        assert!(is(
            price(false, 1, 5777, 6224, DEC, DEC, Q_MAX as i128, Q_MAX).unwrap_err(),
            MakerError::InventoryLimit
        ));
    }

    #[test]
    fn round_trip_at_skew_boundary_cannot_profit() {
        let skew = 89u16; // 89 * 5 = 445 <= 447
        let q = Q_MAX;
        let (bid, ask) = quote_bps(&inputs(6000, 0, skew, 0), 447).unwrap();
        let (proceeds, pos) = price(false, q, bid, ask, DEC, DEC, 0, q).unwrap();
        let (bid2, ask2) = quote_bps(&inputs(6000, 0, skew, pos), 447).unwrap();
        let (bought, _) = price(true, proceeds, bid2, ask2, DEC, DEC, pos, q).unwrap();
        assert!(bought < q, "taker got back {bought} for {q}");
    }

    #[test]
    fn improvement_is_strict() {
        assert!(improves_book(5778, 6223, 5777, 6224));
        assert!(!improves_book(5777, 6223, 5777, 6224));
        assert!(!improves_book(5778, 6224, 5777, 6224));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn quote_respects_floor_and_bounds(
            forecast in 0u16..=10_000,
            op_spread in 0u16..=10_000,
            phi in 0u16..=10_000,
            alpha in 0u16..=10_000,
            skew in 0u16..=89,
            position in -(Q_MAX as i128)..=(Q_MAX as i128),
        ) {
            let Ok(floor) = spread_floor_bps(phi, alpha) else { return Ok(()) };
            let Ok((bid, ask)) = quote_bps(&inputs(forecast, op_spread, skew, position), floor) else { return Ok(()) };
            prop_assert!(ask - bid >= floor);
            prop_assert!(ask - bid >= op_spread as u64);
            prop_assert!(ask <= 10_000);
        }

        #[test]
        fn taker_never_better_than_quote(
            forecast in 0u16..=10_000,
            op_spread in 0u16..=10_000,
            amount_in in 1_000u64..=2_000_000_000,
            position in -(Q_MAX as i128)..=(Q_MAX as i128),
        ) {
            let (bid, ask) = quote_bps(&inputs(forecast, op_spread, 0, position), 447).unwrap();
            if let Ok((out, _)) = price(true, amount_in, bid, ask, DEC, DEC, position, Q_MAX) {
                // collateral paid per outcome unit >= ask
                prop_assert!(amount_in as u128 * 10_000 >= out as u128 * ask as u128);
            }
            if let Ok((out, _)) = price(false, amount_in, bid, ask, DEC, DEC, position, Q_MAX) {
                // collateral received per outcome unit <= bid
                prop_assert!(out as u128 * 10_000 <= amount_in as u128 * bid as u128);
            }
        }

        #[test]
        fn position_never_exceeds_limit(
            buying in any::<bool>(),
            amount_in in 1u64..=20_000_000,
            position in -(Q_MAX as i128)..=(Q_MAX as i128),
        ) {
            if let Ok((_, would_be)) = price(buying, amount_in, 5777, 6224, DEC, DEC, position, Q_MAX) {
                prop_assert!(would_be.unsigned_abs() <= Q_MAX as u128);
            }
        }
    }
}
