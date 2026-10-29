// SPDX-License-Identifier: CC0-1.0
pragma solidity ^0.8.24;

/// @title Calibrated Outcome Quoter (read interface for binary outcome token makers)
/// @notice A proprietary automated market maker (AMM) that quotes a binary outcome
///         token against its collateral discloses the forecasting calibration its
///         quotes rest on, and commits onchain to never quote inside the
///         adverse-selection floor that calibration implies.
/// @dev Prices and probabilities are expressed in basis points (bps) of one unit
///      of collateral per unit of outcome token, so 0 <= bps <= 10_000. The floor
///      follows Shah (2026), "Calibrated Agents as Market Makers", Digital Finance,
///      Digital Object Identifier (DOI) 10.1007/s42521-026-00233-z: an agent whose
///      forecasts carry mean absolute error alpha against realized outcomes, facing
///      an informed share phi of order flow, has positive expected profit per fill
///      only if spread > 2 * phi * alpha.
interface ICalibratedOutcomeQuoter {
    /// @notice A calibration record for the forecaster behind a set of quotes.
    /// @param alphaBps Mean absolute error between forecast and realized outcome,
    ///        in bps of probability, measured over `sampleSize` resolved questions.
    /// @param phiBps The informed fraction of order flow the quoter assumes, in bps.
    /// @param sampleSize Number of resolved forecasts the error is measured on.
    /// @param excluded Number of forecasts excluded from the sample (for example
    ///        unparseable model output). Disclosed so that `sampleSize` cannot
    ///        silently shrink to the convenient subset.
    /// @param measuredAt Unix timestamp at which the record was produced. MUST NOT
    ///        be in the future and MUST NOT precede `earliestResolution`.
    /// @param earliestResolution Unix timestamp of the earliest question
    ///        resolution in the sample.
    /// @param trainingCutoff Unix timestamp of the forecaster's knowledge cutoff,
    ///        or 0 if the forecaster publishes none. When nonzero, every question
    ///        in the sample MUST have resolved after it.
    /// @param datasetHash keccak256 of the canonical per-forecast record set
    ///        (question, forecast, outcome, resolution time) the error was
    ///        computed from.
    /// @param datasetURI Uniform Resource Identifier (URI) from which the record
    ///        set can be fetched. Implementations SHOULD NOT read it on the quote
    ///        or swap path, since its length is attester-controlled.
    struct CalibrationRecord {
        uint16 alphaBps;
        uint16 phiBps;
        uint32 sampleSize;
        uint32 excluded;
        uint64 measuredAt;
        uint64 earliestResolution;
        uint64 trainingCutoff;
        bytes32 datasetHash;
        string datasetURI;
    }

    /// @notice Emitted when a forecaster's calibration record is published or
    ///         replaced. Outcome tokens map to a forecaster via `calibrationOf`.
    event CalibrationUpdated(
        bytes32 indexed modelId, uint16 alphaBps, uint16 phiBps, uint32 sampleSize, uint32 excluded, bytes32 datasetHash
    );

    /// @notice The calibration record that governs quotes on `outcomeToken`.
    /// @return modelId Identifier of the forecaster the record describes.
    /// @return record The record.
    function calibrationOf(address outcomeToken)
        external
        view
        returns (bytes32 modelId, CalibrationRecord memory record);

    /// @notice Minimum spread the quoter commits to on `outcomeToken`, in bps.
    /// @dev MUST equal ceil(2 * phiBps * alphaBps / 10_000). MUST revert under
    ///      the same conditions as `quoteBps`, so a caller never reads a floor
    ///      the quoter will not honour.
    function spreadFloorBps(address outcomeToken) external view returns (uint256);

    /// @notice The current two-sided quote on `outcomeToken`, in bps per unit.
    /// @dev MUST satisfy askBps - bidBps >= spreadFloorBps(outcomeToken).
    ///      MUST revert if no valid calibration record is in force.
    function quoteBps(address outcomeToken) external view returns (uint256 bidBps, uint256 askBps);

    /// @notice Current inventory in `outcomeToken` and the limit the quoter enforces.
    /// @return position Net outcome tokens from fills and operator deposits and
    ///         withdrawals, in base units. Positive means the quoter is long the
    ///         outcome. Tokens that arrive by other routes are not counted.
    /// @return limit Absolute position beyond which the quoter refuses to add exposure.
    function inventoryOf(address outcomeToken) external view returns (int256 position, uint256 limit);

    /// @notice Whether the quoter's current quote tightens both sides of a book
    ///         whose best bid and ask are `refBidBps` and `refAskBps`.
    /// @dev Returns true iff bidBps > refBidBps and askBps < refAskBps, the
    ///      two-sided improvement condition of the paper's Theorem 4.6.
    function improvesBook(address outcomeToken, uint256 refBidBps, uint256 refAskBps) external view returns (bool);
}
