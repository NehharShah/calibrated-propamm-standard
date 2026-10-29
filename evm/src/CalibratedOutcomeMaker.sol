// SPDX-License-Identifier: CC0-1.0
pragma solidity ^0.8.24;

import {IPropAMMRouter} from "./interfaces/IPropAMMRouter.sol";
import {ICalibratedOutcomeQuoter} from "./interfaces/ICalibratedOutcomeQuoter.sol";

interface IERC20Minimal {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
    function decimals() external view returns (uint8);
}

/// @title CalibratedOutcomeMaker
/// @notice A proprietary automated market maker (AMM) for binary outcome tokens.
///         It exposes an exact-input quote and swap interface for routers and a
///         calibrated outcome quoter interface (calibration disclosure, enforced
///         spread floor, inventory limits, inventory-skewed quotes).
/// @dev One contract serves many markets. A market is the pair (collateral, YES
///      outcome token). Quotes are linear in size: every unit trades at the same
///      bid or ask. Outcome tokens are assumed to settle at 1 collateral unit per
///      token if the outcome occurs and 0 otherwise; settlement itself is handled
///      by the outcome token's own framework and is out of scope here.
///
///      All probabilities and prices are in basis points (bps), 0 to 10_000.
///      Pricing follows Shah (2026), Digital Finance, Digital Object Identifier
///      (DOI) 10.1007/s42521-026-00233-z:
///        floor   = ceil(2 * phi * alpha)                (Corollary 4.7 profitability)
///        spread  = max(operatorSpread, floor)
///        mid     = forecast - beta * position           (Section 6.5 inventory skew)
///        bid     = clamp(mid - floor(spread / 2), 0, 10_000 - spread)
///        ask     = bid + spread
///      so for an odd spread the ask sits one bp further from mid than the bid.
///      The maker refuses any fill that would carry |position| above the limit,
///      and refuses to quote at all when the skew across the full limit would
///      exceed the spread (a taker could otherwise round-trip for a profit).
///
///      Two quote paths share the router surface and the same floor and
///      inventory checks. With empty `quoteData` the quote comes from stored
///      parameters the operator updates by transaction. With non-empty
///      `quoteData`, the data is an ABI-encoded (SignedQuote, signature) pair
///      produced offchain by `QUOTE_SIGNER`; the contract verifies the signature
///      and refuses any signed spread narrower than the floor. The second path
///      lets a quoting engine refresh prices at its own speed while the chain
///      still enforces the commitments.
contract CalibratedOutcomeMaker is IPropAMMRouter, ICalibratedOutcomeQuoter {
    uint256 public constant BPS = 10_000;
    int256 internal constant IBPS = 10_000;
    uint8 internal constant MAX_DECIMALS = 36;

    /// @dev Packed into four storage slots: slot 0 holds collateral plus the six
    ///      small fields, then modelId, inventoryLimit, position.
    struct Market {
        address collateral;
        uint16 forecastBps; // forecaster's probability estimate p_hat
        uint16 operatorSpreadBps; // spread the operator would like; floor may override
        uint16 skewBpsPerUnit; // beta: bps of mid shift per whole outcome token held
        uint8 outcomeDecimals;
        uint8 collateralDecimals;
        bool active;
        bytes32 modelId;
        uint256 inventoryLimit; // Q_max in outcome token base units
        int256 position; // net outcome tokens from fills and operator deposits, base units
    }

    /// @notice Stored-parameter quote inputs, settable one market at a time or in a batch.
    struct QuoteParams {
        uint16 forecastBps;
        uint16 operatorSpreadBps;
        uint16 skewBpsPerUnit;
        uint256 inventoryLimit;
    }

    /// @notice A quote produced offchain and signed by `QUOTE_SIGNER` (EIP-712).
    /// @param outcomeToken The market the quote is for.
    /// @param bidBps Price at which the maker buys the outcome token.
    /// @param askBps Price at which the maker sells it. `askBps - bidBps` MUST be
    ///        at least the market's current floor or the quote is refused.
    /// @param maxOutcomeOut Most outcome tokens the maker will sell on this quote, base units.
    /// @param maxOutcomeIn Most outcome tokens the maker will buy on this quote, base units.
    /// @param expiry Unix time after which the quote is refused.
    /// @param nonce Unique per quote; consumed on the first fill.
    struct SignedQuote {
        address outcomeToken;
        uint16 bidBps;
        uint16 askBps;
        uint256 maxOutcomeOut;
        uint256 maxOutcomeIn;
        uint64 expiry;
        uint256 nonce;
    }

    /// @dev Result of pricing a fill, returned in memory to keep the stack shallow.
    struct Priced {
        bool buyingOutcome;
        bool signed;
        uint256 amountOut;
        uint256 nonce;
    }

    bytes32 public constant QUOTE_TYPEHASH = keccak256(
        "SignedQuote(address outcomeToken,uint16 bidBps,uint16 askBps,uint256 maxOutcomeOut,uint256 maxOutcomeIn,uint64 expiry,uint256 nonce)"
    );
    bytes32 internal constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    bytes32 internal constant NAME_HASH = keccak256("CalibratedOutcomeMaker");
    bytes32 internal constant VERSION_HASH = keccak256("1");
    /// @dev secp256k1 group order halved; signatures with a larger `s` are malleable.
    uint256 internal constant HALF_N = 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0;

    address public immutable OPERATOR;
    address public immutable ATTESTER;
    /// @notice Key the offchain quoting engine signs quotes with. Zero disables the signed path.
    address public immutable QUOTE_SIGNER;
    uint64 public immutable MAX_RECORD_AGE; // seconds a calibration record stays valid
    uint32 public immutable MIN_SAMPLE_SIZE;

    mapping(bytes32 => CalibrationRecord) internal _records;
    mapping(address => Market) internal _markets; // keyed by outcome token
    mapping(address => bool) internal _isCollateral;
    /// @notice Signed-quote nonces that have been filled.
    mapping(uint256 => bool) public nonceUsed;

    uint256 private _locked = 1;

    error NotOperator();
    error NotAttester();
    error Reentrancy();
    error UnknownMarket();
    error MarketExists();
    error TokenRoleCollision();
    error BadDecimals(uint8 decimals);
    error BadPair();
    error ZeroRecipient();
    error ZeroAmount();
    error ZeroOutput();
    error ZeroPrice();
    error Expired();
    error Slippage(uint256 amountOut, uint256 amountOutMin);
    error NoCalibration();
    error CalibrationStale(uint64 measuredAt, uint64 maxAge);
    error BadRecordTime(uint64 measuredAt, uint64 earliestResolution);
    error SampleTooSmall(uint32 sampleSize, uint32 minimum);
    error SampleBeforeCutoff(uint64 earliestResolution, uint64 trainingCutoff);
    error InventoryLimit(int256 wouldBe, uint256 limit);
    error InsufficientInventory(uint256 have, uint256 want);
    error TransferFailed(address token);
    error BadBps();
    error FloorOutOfRange(uint256 floorBps);
    error SkewExceedsSpread(uint256 maxSkewBps, uint256 spreadBps);
    error ValueTooLarge();
    error LengthMismatch();
    error QuoteMarketMismatch(address quoted, address requested);
    error QuoteExpired(uint64 expiry);
    error NonceUsed(uint256 nonce);
    error QuoteInsideFloor(uint256 bidBps, uint256 askBps, uint256 floorBps);
    error QuoteSizeExceeded(uint256 requested, uint256 maximum);
    error BadSignature();
    error BadSigner(address recovered);

    event MarketSet(address indexed outcomeToken, address indexed collateral, bytes32 indexed modelId);
    event MarketActiveSet(address indexed outcomeToken, bool active);
    event QuoteParamsSet(
        address indexed outcomeToken,
        uint16 forecastBps,
        uint16 operatorSpreadBps,
        uint16 skewBpsPerUnit,
        uint256 inventoryLimit
    );
    event SignedQuoteUsed(uint256 indexed nonce, address indexed outcomeToken);

    modifier onlyOperator() {
        _onlyOperator();
        _;
    }

    modifier onlyAttester() {
        _onlyAttester();
        _;
    }

    modifier nonReentrant() {
        _enter();
        _;
        _exit();
    }

    constructor(
        address operator_,
        address attester_,
        address quoteSigner_,
        uint64 maxRecordAge_,
        uint32 minSampleSize_
    ) {
        if (operator_ == address(0) || attester_ == address(0)) revert ZeroRecipient();
        OPERATOR = operator_;
        ATTESTER = attester_;
        QUOTE_SIGNER = quoteSigner_;
        MAX_RECORD_AGE = maxRecordAge_;
        MIN_SAMPLE_SIZE = minSampleSize_;
    }

    // ------------------------------------------------------------------ admin

    /// @notice Configure a market once. A token may be an outcome token in one
    ///         market or a collateral in any number of markets, never both.
    function setMarket(address outcomeToken, address collateral, bytes32 modelId) external onlyOperator {
        Market storage m = _markets[outcomeToken];
        if (m.collateral != address(0)) revert MarketExists();
        if (outcomeToken == collateral || _isCollateral[outcomeToken] || _markets[collateral].collateral != address(0)) revert TokenRoleCollision();

        uint8 outDec = IERC20Minimal(outcomeToken).decimals();
        uint8 colDec = IERC20Minimal(collateral).decimals();
        if (outDec > MAX_DECIMALS) revert BadDecimals(outDec);
        if (colDec > MAX_DECIMALS) revert BadDecimals(colDec);

        m.collateral = collateral;
        m.modelId = modelId;
        m.outcomeDecimals = outDec;
        m.collateralDecimals = colDec;
        m.active = true;
        _isCollateral[collateral] = true;
        emit MarketSet(outcomeToken, collateral, modelId);
    }

    /// @notice Pause or resume quoting on a market. Inactive markets fail fast
    ///         with `BadPair` so routers pay little to discover they are closed.
    function setActive(address outcomeToken, bool active) external onlyOperator {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        m.active = active;
        emit MarketActiveSet(outcomeToken, active);
    }

    function setQuoteParams(
        address outcomeToken,
        uint16 forecastBps,
        uint16 operatorSpreadBps,
        uint16 skewBpsPerUnit,
        uint256 inventoryLimit
    ) external onlyOperator {
        _setQuoteParams(
            outcomeToken,
            QuoteParams({
                forecastBps: forecastBps,
                operatorSpreadBps: operatorSpreadBps,
                skewBpsPerUnit: skewBpsPerUnit,
                inventoryLimit: inventoryLimit
            })
        );
    }

    /// @notice Update many markets in one transaction. For a long tail of thin
    ///         markets this is the cost that scales, so it is one call, not many.
    function setQuoteParamsBatch(address[] calldata outcomeTokens, QuoteParams[] calldata params)
        external
        onlyOperator
    {
        if (outcomeTokens.length != params.length) revert LengthMismatch();
        for (uint256 i = 0; i < outcomeTokens.length; ++i) {
            _setQuoteParams(outcomeTokens[i], params[i]);
        }
    }

    /// @notice Publish or replace the calibration record for a forecaster.
    /// @dev Enforces the paper's methodological requirements onchain: the sample
    ///      must be large enough to mean something, every question in it must
    ///      have resolved before the record was produced and, when a training
    ///      cutoff is known, after that cutoff (otherwise the measured error is
    ///      partly recall, not forecasting). A record dated in the future would
    ///      defeat the age limit, so that is refused too.
    function setCalibration(bytes32 modelId, CalibrationRecord calldata r) external onlyAttester {
        if (r.alphaBps > BPS || r.phiBps > BPS) revert BadBps();
        if (r.sampleSize < MIN_SAMPLE_SIZE || r.sampleSize == 0) revert SampleTooSmall(r.sampleSize, MIN_SAMPLE_SIZE);
        if (r.measuredAt > block.timestamp || r.earliestResolution > r.measuredAt) {
            revert BadRecordTime(r.measuredAt, r.earliestResolution);
        }
        if (r.trainingCutoff != 0 && r.earliestResolution <= r.trainingCutoff) {
            revert SampleBeforeCutoff(r.earliestResolution, r.trainingCutoff);
        }
        _records[modelId] = r;
        emit CalibrationUpdated(modelId, r.alphaBps, r.phiBps, r.sampleSize, r.excluded, r.datasetHash);
    }

    /// @notice Operator funds inventory. Deposits of outcome tokens raise
    ///         `position`; this is how the maker gets shares to sell.
    function depositOutcome(address outcomeToken, uint256 amount) external onlyOperator {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        _pull(outcomeToken, msg.sender, amount);
        m.position += _toInt(amount);
    }

    function depositCollateral(address outcomeToken, uint256 amount) external onlyOperator {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        _pull(m.collateral, msg.sender, amount);
    }

    /// @notice Operator withdraws any token. Withdrawing an outcome token lowers
    ///         its market's `position`; withdrawing collateral changes nothing.
    function withdraw(address token, address to, uint256 amount) external onlyOperator {
        Market storage m = _markets[token];
        if (m.collateral != address(0)) m.position -= _toInt(amount);
        _push(token, to, amount);
    }

    // ------------------------------------------- ICalibratedOutcomeQuoter views

    /// @inheritdoc ICalibratedOutcomeQuoter
    function calibrationOf(address outcomeToken)
        external
        view
        returns (bytes32 modelId, CalibrationRecord memory record)
    {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        return (m.modelId, _records[m.modelId]);
    }

    /// @inheritdoc ICalibratedOutcomeQuoter
    /// @dev Reverts under the same conditions as `quoteBps` so a router never
    ///      reads a floor the quoter will not honour.
    function spreadFloorBps(address outcomeToken) external view returns (uint256) {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        return _validFloor(m.modelId);
    }

    /// @inheritdoc ICalibratedOutcomeQuoter
    /// @dev The stored-parameter quote. A signed quote may differ, but never by
    ///      less than the floor.
    function quoteBps(address outcomeToken) public view returns (uint256 bidBps, uint256 askBps) {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        return _quote(m, _validFloor(m.modelId));
    }

    /// @inheritdoc ICalibratedOutcomeQuoter
    function inventoryOf(address outcomeToken) external view returns (int256 position, uint256 limit) {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        return (m.position, m.inventoryLimit);
    }

    /// @inheritdoc ICalibratedOutcomeQuoter
    function improvesBook(address outcomeToken, uint256 refBidBps, uint256 refAskBps) external view returns (bool) {
        (uint256 bid, uint256 ask) = quoteBps(outcomeToken);
        return bid > refBidBps && ask < refAskBps;
    }

    // ------------------------------------------------------------- signed quotes

    /// @notice EIP-712 domain separator, computed per call so a chain fork cannot
    ///         replay quotes signed for the original chain.
    function domainSeparator() public view returns (bytes32) {
        return keccak256(abi.encode(DOMAIN_TYPEHASH, NAME_HASH, VERSION_HASH, block.chainid, address(this)));
    }

    /// @notice The digest `QUOTE_SIGNER` signs for a quote. Exposed so a quoting
    ///         engine can be checked against the contract.
    function quoteDigest(SignedQuote memory q) public view returns (bytes32) {
        bytes32 structHash = keccak256(
            abi.encode(
                QUOTE_TYPEHASH, q.outcomeToken, q.bidBps, q.askBps, q.maxOutcomeOut, q.maxOutcomeIn, q.expiry, q.nonce
            )
        );
        return keccak256(abi.encodePacked("\x19\x01", domainSeparator(), structHash));
    }

    // ------------------------------------------------------------- router

    /// @inheritdoc IPropAMMRouter
    /// @dev Empty `quoteData` prices from stored parameters. Otherwise it is
    ///      `abi.encode(SignedQuote, bytes signature)` and is echoed back as
    ///      `swapData` so a router can pass it straight to `swap`.
    function getAmountOut(address tokenIn, address tokenOut, uint256 amountIn, bytes calldata quoteData)
        external
        view
        returns (uint256 amountOut, bytes memory swapData)
    {
        Priced memory p = _price(tokenIn, tokenOut, amountIn, quoteData);
        return (p.amountOut, quoteData);
    }

    /// @inheritdoc IPropAMMRouter
    function swap(
        address tokenIn,
        address tokenOut,
        address to,
        uint256 amountIn,
        uint256 amountOutMin,
        uint256 deadline,
        bytes calldata swapData
    ) external nonReentrant returns (uint256 amountOut) {
        if (to == address(0)) revert ZeroRecipient();
        if (block.timestamp > deadline) revert Expired();

        Priced memory p = _price(tokenIn, tokenOut, amountIn, swapData);
        amountOut = p.amountOut;
        if (amountOut < amountOutMin) revert Slippage(amountOut, amountOutMin);

        Market storage m = p.buyingOutcome ? _markets[tokenOut] : _markets[tokenIn];
        if (p.buyingOutcome) {
            // Taker buys YES at the ask: we deliver outcome tokens, position falls.
            m.position -= _toInt(amountOut);
        } else {
            // Taker sells YES at the bid: we receive outcome tokens, position rises.
            m.position += _toInt(amountIn);
        }
        if (p.signed) {
            nonceUsed[p.nonce] = true;
            emit SignedQuoteUsed(p.nonce, p.buyingOutcome ? tokenOut : tokenIn);
        }

        _pull(tokenIn, msg.sender, amountIn);
        _push(tokenOut, to, amountOut);
        emit PropAMMSwap(msg.sender, to, tokenIn, tokenOut, amountIn, amountOut);
    }

    // ---------------------------------------------------------------- internal

    function _setQuoteParams(address outcomeToken, QuoteParams memory p) internal {
        Market storage m = _markets[outcomeToken];
        if (m.collateral == address(0)) revert UnknownMarket();
        if (p.forecastBps > BPS || p.operatorSpreadBps > BPS) revert BadBps();
        m.forecastBps = p.forecastBps;
        m.operatorSpreadBps = p.operatorSpreadBps;
        m.skewBpsPerUnit = p.skewBpsPerUnit;
        m.inventoryLimit = p.inventoryLimit;
        emit QuoteParamsSet(outcomeToken, p.forecastBps, p.operatorSpreadBps, p.skewBpsPerUnit, p.inventoryLimit);
    }

    /// @dev Validates the record and returns its floor, touching only the first
    ///      storage slot of the record so the cost does not grow with the URI.
    function _validFloor(bytes32 modelId) internal view returns (uint256 floorBps) {
        CalibrationRecord storage r = _records[modelId];
        uint32 n = r.sampleSize;
        if (n == 0) revert NoCalibration();
        uint64 measuredAt = r.measuredAt;
        if (block.timestamp > uint256(measuredAt) + MAX_RECORD_AGE) {
            revert CalibrationStale(measuredAt, MAX_RECORD_AGE);
        }
        // ceil(2 * phi * alpha / BPS), all in bps of probability
        uint256 num = 2 * uint256(r.phiBps) * uint256(r.alphaBps);
        floorBps = (num + BPS - 1) / BPS;
        // A floor wider than the whole probability range cannot be honoured:
        // refuse to quote rather than quote inside it.
        if (floorBps > BPS) revert FloorOutOfRange(floorBps);
    }

    function _quote(Market storage m, uint256 floor) internal view returns (uint256 bid, uint256 ask) {
        uint256 spread = m.operatorSpreadBps;
        if (spread < floor) spread = floor;

        uint256 unit = 10 ** uint256(m.outcomeDecimals);
        uint256 skew = m.skewBpsPerUnit;
        // Round-trip safety: a taker who sells the full limit and buys it back
        // profits by (skew * limit - spread) per unit, so the skew excursion
        // across the whole limit must not exceed the spread.
        uint256 maxSkew = (skew * m.inventoryLimit) / unit;
        if (maxSkew > spread) revert SkewExceedsSpread(maxSkew, spread);

        // Inventory skew (Avellaneda-Stoikov style): shift mid against the side we hold.
        int256 mid = int256(uint256(m.forecastBps)) - (_toInt(skew) * m.position) / _toInt(unit);

        // Clamp the bid into [0, BPS - spread]; the ask follows at bid + spread.
        int256 lo = mid - _toInt(spread / 2);
        int256 maxLo = IBPS - _toInt(spread);
        if (lo < 0) lo = 0;
        if (lo > maxLo) lo = maxLo;
        // lo is in [0, BPS - spread] here, so both casts are exact.
        // forge-lint: disable-next-line(unsafe-typecast)
        bid = uint256(lo);
        ask = bid + spread;
    }

    /// @dev Decodes and verifies a signed quote against the market's floor. The
    ///      size caps are checked by the caller once the fill size is known.
    function _verifiedQuote(bytes calldata data, address outcomeToken, uint256 floor)
        internal
        view
        returns (SignedQuote memory q)
    {
        bytes memory sig;
        (q, sig) = abi.decode(data, (SignedQuote, bytes));
        if (q.outcomeToken != outcomeToken) revert QuoteMarketMismatch(q.outcomeToken, outcomeToken);
        if (block.timestamp > q.expiry) revert QuoteExpired(q.expiry);
        if (nonceUsed[q.nonce]) revert NonceUsed(q.nonce);
        if (q.askBps > BPS || q.askBps < q.bidBps || q.askBps - q.bidBps < floor) {
            revert QuoteInsideFloor(q.bidBps, q.askBps, floor);
        }
        address signer = _recover(quoteDigest(q), sig);
        if (signer == address(0) || signer != QUOTE_SIGNER) revert BadSigner(signer);
    }

    /// @dev Prices an exact input swap from either quote path and validates the
    ///      inventory limit, the vault balance and, for signed quotes, the size cap.
    function _price(address tokenIn, address tokenOut, uint256 amountIn, bytes calldata data)
        internal
        view
        returns (Priced memory p)
    {
        Market storage m = _markets[tokenOut];
        if (m.active && m.collateral == tokenIn) {
            p.buyingOutcome = true;
        } else {
            m = _markets[tokenIn];
            if (!(m.active && m.collateral == tokenOut)) revert BadPair();
        }
        if (amountIn == 0) revert ZeroAmount();

        uint256 floor = _validFloor(m.modelId);
        uint256 bid;
        uint256 ask;
        SignedQuote memory q;
        if (data.length == 0) {
            (bid, ask) = _quote(m, floor);
        } else {
            q = _verifiedQuote(data, p.buyingOutcome ? tokenOut : tokenIn, floor);
            (bid, ask) = (q.bidBps, q.askBps);
            p.signed = true;
            p.nonce = q.nonce;
        }

        p.amountOut = _fill(m, p.buyingOutcome, amountIn, bid, ask, tokenOut);

        if (p.signed) {
            if (p.buyingOutcome && p.amountOut > q.maxOutcomeOut) {
                revert QuoteSizeExceeded(p.amountOut, q.maxOutcomeOut);
            }
            if (!p.buyingOutcome && amountIn > q.maxOutcomeIn) revert QuoteSizeExceeded(amountIn, q.maxOutcomeIn);
        }
    }

    /// @dev The part of pricing that does not depend on where the quote came from.
    function _fill(Market storage m, bool buyingOutcome, uint256 amountIn, uint256 bid, uint256 ask, address tokenOut)
        internal
        view
        returns (uint256 amountOut)
    {
        uint256 outUnit = 10 ** uint256(m.outcomeDecimals);
        uint256 colUnit = 10 ** uint256(m.collateralDecimals);
        int256 wouldBe;

        if (buyingOutcome) {
            if (ask == 0) revert ZeroPrice();
            // outcome tokens = collateral / (ask/BPS), with decimal conversion
            amountOut = (amountIn * BPS * outUnit) / (ask * colUnit);
            wouldBe = m.position - _toInt(amountOut);
        } else {
            amountOut = (amountIn * bid * colUnit) / (BPS * outUnit);
            wouldBe = m.position + _toInt(amountIn);
        }

        if (_abs(wouldBe) > m.inventoryLimit) revert InventoryLimit(wouldBe, m.inventoryLimit);
        if (amountOut == 0) revert ZeroOutput();
        uint256 have = IERC20Minimal(tokenOut).balanceOf(address(this));
        if (amountOut > have) revert InsufficientInventory(have, amountOut);
    }

    /// @dev ecrecover with the usual guards: 65-byte signature, canonical `s`,
    ///      `v` in {27, 28}. Returns zero for an invalid signature; callers reject zero.
    function _recover(bytes32 digest, bytes memory sig) internal pure returns (address) {
        if (sig.length != 65) revert BadSignature();
        bytes32 r;
        bytes32 s;
        uint8 v;
        // solhint-disable-next-line no-inline-assembly
        assembly ("memory-safe") {
            r := mload(add(sig, 0x20))
            s := mload(add(sig, 0x40))
            v := byte(0, mload(add(sig, 0x60)))
        }
        if (v < 27) v += 27;
        if (v != 27 && v != 28) revert BadSignature();
        if (uint256(s) > HALF_N) revert BadSignature();
        return ecrecover(digest, v, r, s);
    }

    function _enter() internal {
        if (_locked != 1) revert Reentrancy();
        _locked = 2;
    }

    function _exit() internal {
        _locked = 1;
    }

    function _onlyOperator() internal view {
        if (msg.sender != OPERATOR) revert NotOperator();
    }

    function _onlyAttester() internal view {
        if (msg.sender != ATTESTER) revert NotAttester();
    }

    function _toInt(uint256 x) internal pure returns (int256) {
        if (x > uint256(type(int256).max)) revert ValueTooLarge();
        // forge-lint: disable-next-line(unsafe-typecast)
        return int256(x);
    }

    function _abs(int256 x) internal pure returns (uint256) {
        // The negation of int256.min is checked arithmetic and reverts; every
        // other value fits in uint256 after the sign is removed.
        // forge-lint: disable-next-line(unsafe-typecast)
        return x < 0 ? uint256(-x) : uint256(x);
    }

    function _pull(address token, address from, uint256 amount) internal {
        if (!IERC20Minimal(token).transferFrom(from, address(this), amount)) revert TransferFailed(token);
    }

    function _push(address token, address to, uint256 amount) internal {
        if (!IERC20Minimal(token).transfer(to, amount)) revert TransferFailed(token);
    }
}
