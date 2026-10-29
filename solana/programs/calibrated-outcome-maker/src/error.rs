use anchor_lang::prelude::*;

/// Error names mirror the Solidity implementation in `../../evm` one for one,
/// so a reader can cross-check the two.
#[error_code]
pub enum MakerError {
    #[msg("signer is not the operator")]
    NotOperator,
    #[msg("signer is not the attester")]
    NotAttester,
    #[msg("token pair is not an active market in this direction")]
    BadPair,
    #[msg("a token may be an outcome token in one market or a collateral, never both")]
    TokenRoleCollision,
    #[msg("mint decimals exceed the supported maximum")]
    BadDecimals,
    #[msg("zero account where one is required")]
    ZeroAddress,
    #[msg("amount in must be nonzero")]
    ZeroAmount,
    #[msg("the fill would deliver nothing")]
    ZeroOutput,
    #[msg("ask of zero cannot be filled")]
    ZeroPrice,
    #[msg("deadline has passed")]
    Expired,
    #[msg("amount out is below the minimum")]
    Slippage,
    #[msg("no calibration record is published for this market's model")]
    NoCalibration,
    #[msg("calibration record is older than the maximum age")]
    CalibrationStale,
    #[msg("measuredAt is in the future or precedes earliestResolution")]
    BadRecordTime,
    #[msg("sample size is below the minimum")]
    SampleTooSmall,
    #[msg("earliest resolution is not after the training cutoff")]
    SampleBeforeCutoff,
    #[msg("the fill would carry the position beyond the inventory limit")]
    InventoryLimit,
    #[msg("vault holds less than the fill requires")]
    InsufficientInventory,
    #[msg("basis points value exceeds 10000")]
    BadBps,
    #[msg("the spread floor exceeds the probability range; the maker refuses to quote")]
    FloorOutOfRange,
    #[msg("skew across the full inventory limit exceeds the spread; a round trip could profit")]
    SkewExceedsSpread,
    #[msg("arithmetic overflow")]
    MathOverflow,
    #[msg("dataset URI exceeds the maximum length")]
    UriTooLong,
    #[msg("remaining accounts and parameters differ in length")]
    LengthMismatch,
    #[msg("market account must be writable")]
    NotWritable,
    #[msg("account is not a market of this maker")]
    UnknownMarket,
    #[msg("signed quote has expired")]
    QuoteExpired,
    #[msg("signed quote is narrower than the floor or crossed")]
    QuoteInsideFloor,
    #[msg("fill exceeds the size the signed quote allows")]
    QuoteSizeExceeded,
    #[msg("co-signer is not the maker's quote signer; a default key disables signed quotes")]
    BadSigner,
}
