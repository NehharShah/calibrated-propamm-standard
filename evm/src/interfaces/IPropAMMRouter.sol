// SPDX-License-Identifier: CC0-1.0
pragma solidity ^0.8.24;

/// @title Proprietary automated market maker (AMM) router interface
/// @notice The exact-input quote and swap surface the maker exposes to routers.
/// @dev A contract may serve one or more markets.
///      `tokenIn` and `tokenOut` specify the requested assets and direction.
///      Implementations may declare `getAmountOut` as `view`; the selector is
///      unchanged.
interface IPropAMMRouter {
    /// @notice Emitted after a successful swap.
    event PropAMMSwap(
        address indexed sender,
        address to,
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        uint256 amountOut
    );

    /// @notice Quotes an exact input swap.
    /// @dev Intentionally non-view. Routers must call it in a frame whose state
    ///      changes are reverted.
    function getAmountOut(address tokenIn, address tokenOut, uint256 amountIn, bytes calldata quoteData)
        external
        returns (uint256 amountOut, bytes memory swapData);

    /// @notice Executes an exact input swap.
    /// @dev `msg.sender` must have approved at least `amountIn` of `tokenIn`.
    function swap(
        address tokenIn,
        address tokenOut,
        address to,
        uint256 amountIn,
        uint256 amountOutMin,
        uint256 deadline,
        bytes calldata swapData
    ) external returns (uint256 amountOut);
}
