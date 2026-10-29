# Calibrated Outcome Maker

Smart contract implementations of the market maker from the paper "Calibrated Agents as Market Makers: Automated Liquidity Provision for Long-Tail Prediction Markets", Digital Finance (Springer), accepted October 2026, Digital Object Identifier (DOI) [10.1007/s42521-026-00233-z](https://doi.org/10.1007/s42521-026-00233-z).

The maker quotes binary outcome tokens against a collateral token. It publishes a calibration record for its forecasting model, derives a spread floor from that record, and never quotes or fills inside the floor. Three results from the paper become contract-level rules:

- Corollary 4.7: a maker with mean absolute forecasting error alpha facing informed share phi profits per fill only if spread > 2 * phi * alpha. The contract computes this floor from the published record and refuses to quote below it.
- Theorem 4.6 (two-sided improvement): a quote improves a book only if it tightens both the bid and the ask. Exposed as an improvement test.
- Section 6.5 (inventory): position limits and skew are the main tail-risk control. Position and limit are readable onchain, the limit is enforced in both directions, and the skew is bounded so that a round trip cannot profit the taker.

## What runs onchain and what does not

The contracts are the enforcement layer, not the quoting engine. Onchain they hold the calibration record, the floor it implies, the inventory ledger and the limit, and they refuse any fill that violates one of them. Where the price sits inside the allowed region is a decision that can be made offchain, at whatever latency the operator's own forecasting and hedging stack runs at. Two quote paths share one swap surface:

- Stored parameters. The operator writes a forecast, an operator spread and a skew per market, and the contract computes the quote from them. No offchain component is needed to trade; this is the fallback and the long-tail path.
- Signed quotes. An offchain engine produces a bid, an ask, size caps and an expiry per fill and authorises them: on the EVM by signing an EIP-712 (Ethereum Improvement Proposal 712, typed structured data) message that the taker passes in the swap calldata; on Solana by co-signing the taker's transaction. The contract accepts the engine's price only after the same checks the stored path runs: the spread is at least the floor from the current record, the record is fresh, the fill respects the inventory limit and the vault balance, and the quote's own size cap and expiry hold. The engine chooses the price; the contract bounds it.

Parameter updates for the stored path are batched (`setQuoteParamsBatch`, `set_quote_params_batch`) so that an operator running many thin markets refreshes them in one transaction.

## Implementations

| Directory | Chain | Language and tooling | Tests as of 2026-10-29 |
| --- | --- | --- | --- |
| [`evm/`](./evm/README.md) | Ethereum Virtual Machine (EVM) chains | Solidity 0.8.33, Foundry | 58 unit tests and 4 fuzz properties |
| [`solana/`](./solana/README.md) | Solana | Rust, Anchor 1.x, LiteSVM | 11 unit tests, 3 property tests and 24 integration tests |

Both share the same pricing arithmetic, record validity rules, error names and inventory controls. Each directory's README has its build steps, a list of what the tests cover, and the limitations specific to that chain.

## Licence

Released under CC0-1.0.
