# Calibrated Outcome Maker (EVM)

Solidity implementation of the market maker from the paper "Calibrated Agents as Market Makers: Automated Liquidity Provision for Long-Tail Prediction Markets", Digital Finance (Springer), accepted October 2026, Digital Object Identifier (DOI) [10.1007/s42521-026-00233-z](https://doi.org/10.1007/s42521-026-00233-z).

The maker quotes binary outcome tokens against a collateral token. It publishes a calibration record for its forecasting model, derives a spread floor from that record, and never quotes or fills inside the floor. Three results from the paper become contract-level rules:

- Corollary 4.7: a maker with mean absolute forecasting error alpha facing informed share phi profits per fill only if spread > 2 * phi * alpha. The contract computes this floor from the published record and refuses to quote below it.
- Theorem 4.6 (two-sided improvement): a quote improves a book only if it tightens both the bid and the ask. Exposed as `improvesBook()`.
- Section 6.5 (inventory): position limits and skew are the main tail-risk control. Position and limit are readable onchain, the limit is enforced in both directions, and the skew is bounded so that a round trip cannot profit the taker.

## Files

| Path | What it is |
| --- | --- |
| `src/CalibratedOutcomeMaker.sol` | The maker: one contract serving many (collateral, outcome token) markets with linear quotes, an operator-set forecast and spread, inventory skew with a round-trip guard, a position limit, a reentrancy guard, a pause switch per market, an attester-set calibration record, batch parameter updates, and an optional signed-quote path for an offchain quoting engine. |
| `src/interfaces/ICalibratedOutcomeQuoter.sol` | The read interface: calibration record, spread floor, quote, inventory and improvement test. Its NatSpec states the record validity rules and the quote and execution invariants. |
| `src/interfaces/IPropAMMRouter.sol` | The exact-input quote and swap interface the maker exposes to routers. |
| `test/CalibratedOutcomeMaker.t.sol` | Foundry tests: 58 unit tests and 4 fuzz properties as of 2026-10-29, all passing. |
| `foundry.toml` | Compiler, EVM version and optimizer pinned. `lib/forge-std` is installed by the step below and not committed. |

## Build and test

Tested with forge 1.5.1 and solc 0.8.33 as of 2026-10-29.

```
export PATH="$HOME/.foundry/bin:$PATH"
forge install foundry-rs/forge-std@v1.17.0 --no-git   # fresh checkout only
forge build
forge test
forge fmt --check && forge lint
```

## Two quote paths, one swap surface

`getAmountOut(tokenIn, tokenOut, amountIn, quoteData)` and `swap(..., swapData)` are the router surface. The bytes argument selects the path:

- Empty: the quote is computed onchain from the market's stored parameters (forecast, operator spread, skew, position). Nothing offchain is required.
- Non-empty: it is `abi.encode(SignedQuote, signature)`, a quote an offchain engine produced and signed with the `QUOTE_SIGNER` key fixed at deployment. `getAmountOut` echoes the bytes back as `swapData`, so a router forwards them unchanged.

A signed quote is `(outcomeToken, bidBps, askBps, maxOutcomeOut, maxOutcomeIn, expiry, nonce)` under an EIP-712 (Ethereum Improvement Proposal 712, typed structured data) domain `CalibratedOutcomeMaker` version `1` bound to the chain id and the contract address. `quoteDigest(q)` returns the digest the engine signs. The contract accepts the engine's bid and ask only if `ask - bid` is at least the floor the current record implies and the record is fresh, then prices the fill with the same arithmetic as the stored path, applies the inventory limit and vault balance checks, and finally holds the fill to the quote's size cap. Each nonce fills once (`SignedQuoteUsed` is emitted). Signatures must be 65 bytes with a low `s` value; a zero `QUOTE_SIGNER` disables the path.

Division of labour: the contract enforces the floor, the inventory limit and the record validity; the engine decides where inside that region the price sits, at its own latency, and never needs a transaction to move a quote. `setQuoteParamsBatch` refreshes the stored parameters of many markets in one transaction for the long tail that is not worth an engine.

## What the tests cover

- Floor arithmetic from the paper's numbers (calibration examples as of June 2026): alpha 0.149 and phi 0.15 give 447 bps; alpha 0.354 gives 1062 bps; rounding is upward.
- Refusal to quote when the floor exceeds 10 000 bps or when the skew across the full inventory limit exceeds the spread, from every entry point.
- Quote invariant at both clamp bounds, with a full-width spread, with skew pushing the midpoint below zero, and fuzzed over forecast, spread, calibration and position.
- Execution invariant: the taker never fills inside the posted quote, and `swap` delivers exactly what `getAmountOut` quoted, in both directions, fuzzed.
- Deadline, zero recipient, slippage, bad pair, zero input and zero output reverts; `InsufficientInventory` and `TransferFailed` surfaced with their data.
- Record rejection with the exact error: missing, stale, sample too small, basis points out of range, future `measuredAt`, resolution after measurement, resolution at or before the training cutoff.
- Inventory limit in both directions to the wei, plus a fuzz property that no successful swap leaves |position| above the limit.
- A sell-then-buy round trip of the full limit at the skew boundary that cannot extract collateral.
- A collateral token that re-enters `swap` during `transferFrom` is rejected.
- Market setup rejects repeats, self-pairs, token-role collisions and tokens with more than 36 decimals; only the operator configures; the constructor rejects zero roles.
- Two-sided improvement including the strict boundary.
- Signed quotes: fills in both directions at the signed price and `getAmountOut` echoing the quote bytes; nonce consumed and `SignedQuoteUsed` emitted; 446 bps rejected, 447 accepted and a crossed quote rejected; the floor following a record change to 1062; a stale record refusing signed quotes; expiry, nonce reuse, market mismatch, wrong signer and tampered data each rejected; 64-byte, high-`s` and bad-`v` signatures rejected; size caps exact to the wei; the inventory limit binding regardless of the quote; the path disabled when the signer is zero; the domain separator; a fuzz property that no accepted signed quote is narrower than the floor.
- Batch updates set two markets in one call and reject length mismatch, an unknown market and a non-operator.

Gas as of 2026-10-29 (forge 1.5.1, optimizer 200 runs, median across the suite): `getAmountOut` 23.1k, `quoteBps` 14.3k, `swap` 100.6k; `setQuoteParamsBatch` for two markets 66.1k. Runtime size 11,892 bytes.

## Known limitations

- Self-attested calibration. The attester is a single immutable address; a dishonest attester can understate alpha. The record carries a dataset hash and URI so the number can be recomputed.
- Roles are immutable. A lost attester key means the maker stops quoting once its record ages out, which fails safe.
- Linear pricing. Every unit in a fill trades at the same price.
- `position` is a ledger of fills and operator actions, not a balance. Tokens that arrive by direct transfer are deliverable but uncounted.
- Tokens must return a boolean from transfers. Fee-on-transfer, rebasing and no-return tokens are unsupported.
- Public parameter updates can be sandwiched: a forecast or spread change sits in the mempool before it lands, and a taker can trade against the old quote in the same block. The signed-quote path avoids this because the price never passes through a state change.
- A signed quote is a promise the engine made; the contract does not check that it is better than the stored quote, only that it is outside the floor and inside the inventory limit. A compromised `QUOTE_SIGNER` key can quote at the floor until the operator pauses the market; it cannot quote inside it. The signer is immutable, so rotation means a new deployment.
- Not audited. Do not deploy with real funds.

## Licence

Released under CC0-1.0.
