// SPDX-License-Identifier: CC0-1.0
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {CalibratedOutcomeMaker} from "../src/CalibratedOutcomeMaker.sol";
import {ICalibratedOutcomeQuoter} from "../src/interfaces/ICalibratedOutcomeQuoter.sol";
import {IPropAMMRouter} from "../src/interfaces/IPropAMMRouter.sol";

/// @dev Returns false (instead of reverting) on a short balance or allowance,
///      like the ERC-20 (Ethereum Request for Comment 20) tokens that signal
///      failure through the return value, so `TransferFailed` is reachable.
contract MockERC20 {
    string public name;
    uint8 public decimals;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    constructor(string memory n, uint8 d) {
        name = n;
        decimals = d;
    }

    function mint(address to, uint256 a) external {
        balanceOf[to] += a;
    }

    function approve(address s, uint256 a) external returns (bool) {
        allowance[msg.sender][s] = a;
        return true;
    }

    function transfer(address to, uint256 a) external returns (bool) {
        if (balanceOf[msg.sender] < a) return false;
        balanceOf[msg.sender] -= a;
        balanceOf[to] += a;
        return true;
    }

    function transferFrom(address f, address t, uint256 a) external virtual returns (bool) {
        if (allowance[f][msg.sender] < a || balanceOf[f] < a) return false;
        allowance[f][msg.sender] -= a;
        balanceOf[f] -= a;
        balanceOf[t] += a;
        return true;
    }
}

/// @dev Collateral whose `transferFrom` re-enters the maker once.
contract ReentrantERC20 is MockERC20 {
    CalibratedOutcomeMaker public maker;
    address public outcome;
    bool internal armed;
    bytes public innerRevert;

    constructor() MockERC20("EVIL", 6) {}

    function arm(CalibratedOutcomeMaker m, address o) external {
        maker = m;
        outcome = o;
        armed = true;
    }

    function transferFrom(address f, address t, uint256 a) external override returns (bool) {
        if (armed) {
            armed = false;
            try maker.swap(address(this), outcome, address(0xDEAD), 1e6, 0, block.timestamp, "") {
                innerRevert = "";
            } catch (bytes memory reason) {
                innerRevert = reason;
            }
        }
        if (allowance[f][msg.sender] < a || balanceOf[f] < a) return false;
        allowance[f][msg.sender] -= a;
        balanceOf[f] -= a;
        balanceOf[t] += a;
        return true;
    }
}

contract CalibratedOutcomeMakerTest is Test {
    CalibratedOutcomeMaker maker;
    MockERC20 usd; // 6 decimals collateral
    MockERC20 yes; // 18 decimals outcome token

    address operator = address(0xA11CE);
    address attester = address(0xA77E5);
    address router = address(0xB0B);
    address taker = address(0x7A4E);
    uint256 constant SIGNER_PK = 0xC0FFEE;
    address quoteSigner = vm.addr(SIGNER_PK);

    bytes32 constant MODEL = keccak256("claude-opus-4.6");
    uint256 constant Q_MAX = 5e18; // five outcome tokens, the paper's Q_max = 5
    int256 constant IQ_MAX = 5e18;
    uint256 constant FLOOR = 447; // 2 * 0.15 * 0.149 = 0.0447 (paper Table 4, as of June 2026)
    uint64 constant MAX_AGE = 30 days;
    uint32 constant MIN_N = 15;

    function setUp() public {
        vm.warp(1_800_000_000); // 2027-01-15, a sane clock for age arithmetic
        usd = new MockERC20("USD", 6);
        yes = new MockERC20("YES", 18);
        maker = new CalibratedOutcomeMaker(operator, attester, quoteSigner, MAX_AGE, MIN_N);

        vm.startPrank(operator);
        maker.setMarket(address(yes), address(usd), MODEL);
        // forecast 60%, operator would like a 1% spread, skew 50 bps per token, Q_max = 5
        maker.setQuoteParams(address(yes), 6000, 100, 50, Q_MAX);
        vm.stopPrank();

        // Record from the paper (Table 4, Claude Opus 4.6, 15 resolved questions):
        // mean absolute error 0.149, informed fraction 0.15. Sample resolved after cutoff.
        _setRecord(1490, 1500, 15, 0, _now(), _now() - 1 days, _now() - 30 days);

        // Fund the maker with collateral; outcome inventory is added per test.
        usd.mint(operator, 1_000_000e6);
        yes.mint(operator, 100e18);
        vm.startPrank(operator);
        usd.approve(address(maker), type(uint256).max);
        yes.approve(address(maker), type(uint256).max);
        maker.depositCollateral(address(yes), 1_000_000e6);
        vm.stopPrank();

        // Router holds taker funds and approves the maker before swapping.
        usd.mint(router, 1_000_000e6);
        yes.mint(router, 100e18);
        vm.startPrank(router);
        usd.approve(address(maker), type(uint256).max);
        yes.approve(address(maker), type(uint256).max);
        vm.stopPrank();
    }

    // ------------------------------------------------------------ floor

    function test_floor_is_2_phi_alpha() public view {
        assertEq(maker.spreadFloorBps(address(yes)), FLOOR);
    }

    function test_floor_rounds_up() public {
        // 2 * 1500 * 1 / 10_000 = 0.3 -> ceil = 1 bp
        _setRecord(1, 1500, 15, 0, _now(), _now() - 1 days, 0);
        assertEq(maker.spreadFloorBps(address(yes)), 1);
        // 2 * 1500 * 3 / 10_000 = 0.9 -> 1; 2 * 1500 * 34 / 10_000 = 10.2 -> 11
        _setRecord(34, 1500, 15, 0, _now(), _now() - 1 days, 0);
        assertEq(maker.spreadFloorBps(address(yes)), 11);
    }

    function test_quote_never_inside_floor() public view {
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        // operator asked for 100 bps; the floor wins
        assertEq(ask - bid, FLOOR);
        // bid = mid - floor(447/2), ask = bid + 447 with zero inventory
        assertEq(bid, 6000 - 223);
        assertEq(ask, 6000 - 223 + 447);
    }

    function test_operator_spread_used_when_wider_than_floor() public {
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 6000, 1000, 50, Q_MAX);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(ask - bid, 1000);
    }

    function test_floor_rises_with_worse_calibration() public {
        // Expanded set level from the paper (Table 5, as of June 2026): error 0.354 -> floor 1062 bps
        _setRecord(3540, 1500, 60, 0, _now(), _now() - 1 days, _now() - 30 days);
        assertEq(maker.spreadFloorBps(address(yes)), 1062);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(ask - bid, 1062);
    }

    function test_refuses_to_quote_when_floor_exceeds_range() public {
        // alpha = 1.0, phi = 1.0 gives a floor of 20_000 bps: nothing can honour it.
        _setRecord(10_000, 10_000, 60, 0, _now(), _now() - 1 days, _now() - 30 days);
        bytes memory err = abi.encodeWithSelector(CalibratedOutcomeMaker.FloorOutOfRange.selector, 20_000);
        vm.expectRevert(err);
        maker.spreadFloorBps(address(yes));
        vm.expectRevert(err);
        maker.quoteBps(address(yes));
        vm.expectRevert(err);
        maker.getAmountOut(address(yes), address(usd), 1e18, "");
        vm.prank(router);
        vm.expectRevert(err);
        maker.swap(address(yes), address(usd), taker, 1e18, 0, block.timestamp, "");
    }

    // ------------------------------------------------------ record validity

    function test_reverts_without_record() public {
        MockERC20 other = new MockERC20("NO", 18);
        vm.prank(operator);
        maker.setMarket(address(other), address(usd), keccak256("unknown-model"));
        vm.expectRevert(CalibratedOutcomeMaker.NoCalibration.selector);
        maker.quoteBps(address(other));
        // the floor view reverts the same way, never returning a floor of zero
        vm.expectRevert(CalibratedOutcomeMaker.NoCalibration.selector);
        maker.spreadFloorBps(address(other));
    }

    function test_reverts_when_record_stale() public {
        uint64 measuredAt = _now();
        vm.warp(block.timestamp + MAX_AGE); // exactly at the limit: still valid
        maker.quoteBps(address(yes));
        vm.warp(block.timestamp + 1);
        bytes memory err = abi.encodeWithSelector(CalibratedOutcomeMaker.CalibrationStale.selector, measuredAt, MAX_AGE);
        vm.expectRevert(err);
        maker.quoteBps(address(yes));
        vm.prank(router);
        vm.expectRevert(err);
        maker.swap(address(yes), address(usd), taker, 1e18, 0, block.timestamp, "");
    }

    function test_rejects_sample_before_training_cutoff() public {
        uint64 t = _now();
        vm.prank(attester);
        vm.expectRevert(
            abi.encodeWithSelector(CalibratedOutcomeMaker.SampleBeforeCutoff.selector, t - 40 days, t - 30 days)
        );
        maker.setCalibration(MODEL, _record(700, 1500, 15, 0, t, t - 40 days, t - 30 days));
    }

    function test_rejects_sample_resolved_exactly_at_cutoff() public {
        uint64 t = _now();
        vm.prank(attester);
        vm.expectRevert(
            abi.encodeWithSelector(CalibratedOutcomeMaker.SampleBeforeCutoff.selector, t - 30 days, t - 30 days)
        );
        maker.setCalibration(MODEL, _record(700, 1500, 15, 0, t, t - 30 days, t - 30 days));
    }

    function test_accepts_record_without_training_cutoff() public {
        _setRecord(700, 1500, 15, 0, _now(), _now() - 100 days, 0);
        (bytes32 id, ICalibratedOutcomeQuoter.CalibrationRecord memory r) = maker.calibrationOf(address(yes));
        assertEq(id, MODEL);
        assertEq(r.alphaBps, 700);
        assertEq(r.trainingCutoff, 0);
    }

    function test_rejects_small_sample() public {
        uint64 t = _now();
        vm.prank(attester);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.SampleTooSmall.selector, 4, MIN_N));
        // GPT-5.2 after parse-failure exclusion on the 15-question set (paper, as of June 2026)
        maker.setCalibration(MODEL, _record(700, 1500, 4, 11, t, t, 0));
    }

    function test_rejects_future_measured_at() public {
        uint64 t = _now();
        vm.prank(attester);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.BadRecordTime.selector, t + 1, t));
        maker.setCalibration(MODEL, _record(700, 1500, 15, 0, t + 1, t, 0));
    }

    function test_rejects_resolution_after_measurement() public {
        uint64 t = _now();
        vm.prank(attester);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.BadRecordTime.selector, t - 1, t));
        maker.setCalibration(MODEL, _record(700, 1500, 15, 0, t - 1, t, 0));
    }

    function test_rejects_bps_out_of_range_in_record() public {
        uint64 t = _now();
        vm.prank(attester);
        vm.expectRevert(CalibratedOutcomeMaker.BadBps.selector);
        maker.setCalibration(MODEL, _record(10_001, 1500, 15, 0, t, t, 0));
    }

    function test_only_attester_sets_calibration() public {
        ICalibratedOutcomeQuoter.CalibrationRecord memory r;
        vm.prank(operator);
        vm.expectRevert(CalibratedOutcomeMaker.NotAttester.selector);
        maker.setCalibration(MODEL, r);
    }

    function test_emits_calibration_updated() public {
        uint64 t = _now();
        vm.expectEmit(true, true, true, true);
        emit ICalibratedOutcomeQuoter.CalibrationUpdated(MODEL, 700, 1500, 15, 2, keccak256("backtest_questions.json"));
        _setRecord(700, 1500, 15, 2, t, t - 1 days, 0);
    }

    // --------------------------------------------------------------- swaps

    function test_buy_outcome_at_ask_pulls_exact_input() public {
        _fundOutcomeInventory(3e18);
        (, uint256 ask) = maker.quoteBps(address(yes));
        uint256 amountIn = 1e6; // 1 USD buys about 1.6 outcome tokens at a 62% ask

        (uint256 quoted,) = maker.getAmountOut(address(usd), address(yes), amountIn, "");
        uint256 expected = (amountIn * 10_000 * 1e18) / (ask * 1e6);
        assertEq(quoted, expected);

        uint256 routerUsdBefore = usd.balanceOf(router);
        uint256 takerYesBefore = yes.balanceOf(taker);
        (int256 posBefore,) = maker.inventoryOf(address(yes));

        vm.prank(router);
        vm.expectEmit(true, true, true, true);
        emit IPropAMMRouter.PropAMMSwap(router, taker, address(usd), address(yes), amountIn, expected);
        uint256 out = maker.swap(address(usd), address(yes), taker, amountIn, expected, block.timestamp, "");

        assertEq(out, quoted, "swap delivers exactly what getAmountOut quoted");
        assertEq(routerUsdBefore - usd.balanceOf(router), amountIn, "exact input pulled");
        assertEq(yes.balanceOf(taker) - takerYesBefore, expected, "output credited to `to`");
        (int256 posAfter,) = maker.inventoryOf(address(yes));
        assertEq(posBefore - posAfter, _int(expected), "position falls when selling outcome");
    }

    function test_sell_outcome_at_bid() public {
        (uint256 bid,) = maker.quoteBps(address(yes));
        uint256 amountIn = 2e18; // two outcome tokens
        uint256 expected = (amountIn * bid * 1e6) / (10_000 * 1e18);
        (uint256 quoted,) = maker.getAmountOut(address(yes), address(usd), amountIn, "");
        assertEq(quoted, expected);

        vm.prank(router);
        uint256 out = maker.swap(address(yes), address(usd), taker, amountIn, expected, block.timestamp, "");
        assertEq(out, quoted, "swap delivers exactly what getAmountOut quoted");
        assertEq(usd.balanceOf(taker), expected);
        (int256 pos,) = maker.inventoryOf(address(yes));
        assertEq(pos, _int(amountIn));
    }

    function test_taker_never_trades_inside_quote() public {
        _fundOutcomeInventory(3e18);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        // Buying: effective price paid per outcome token >= ask
        (uint256 outBuy,) = maker.getAmountOut(address(usd), address(yes), 1e6, "");
        uint256 paidBps = (1e6 * 10_000 * 1e18) / (outBuy * 1e6);
        assertGe(paidBps, ask);
        // Selling: effective price received per outcome token <= bid
        (uint256 outSell,) = maker.getAmountOut(address(yes), address(usd), 1e18, "");
        uint256 recvBps = (outSell * 10_000 * 1e18) / (1e18 * 1e6);
        assertLe(recvBps, bid);
    }

    function test_swap_reverts_on_deadline() public {
        vm.prank(router);
        vm.expectRevert(CalibratedOutcomeMaker.Expired.selector);
        maker.swap(address(yes), address(usd), taker, 1e18, 0, block.timestamp - 1, "");
    }

    function test_swap_reverts_on_zero_recipient() public {
        vm.prank(router);
        vm.expectRevert(CalibratedOutcomeMaker.ZeroRecipient.selector);
        maker.swap(address(yes), address(usd), address(0), 1e18, 0, block.timestamp, "");
    }

    function test_swap_reverts_on_slippage() public {
        (uint256 quoted,) = maker.getAmountOut(address(yes), address(usd), 1e18, "");
        vm.prank(router);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.Slippage.selector, quoted, quoted + 1));
        maker.swap(address(yes), address(usd), taker, 1e18, quoted + 1, block.timestamp, "");
    }

    function test_swap_reverts_on_bad_pair() public {
        vm.prank(router);
        vm.expectRevert(CalibratedOutcomeMaker.BadPair.selector);
        maker.swap(address(usd), address(usd), taker, 1e6, 0, block.timestamp, "");
    }

    function test_swap_reverts_on_zero_amount_in() public {
        vm.expectRevert(CalibratedOutcomeMaker.ZeroAmount.selector);
        maker.getAmountOut(address(yes), address(usd), 0, "");
        vm.prank(address(0xDEAD)); // no balance, no approval
        vm.expectRevert(CalibratedOutcomeMaker.ZeroAmount.selector);
        maker.swap(address(yes), address(usd), taker, 0, 0, block.timestamp, "");
    }

    function test_swap_reverts_when_output_rounds_to_zero() public {
        // 1e11 wei of YES at a 57.77% bid is 0.0000000577 USD, below one 6-decimal unit
        vm.expectRevert(CalibratedOutcomeMaker.ZeroOutput.selector);
        maker.getAmountOut(address(yes), address(usd), 1e11, "");
        vm.prank(router);
        vm.expectRevert(CalibratedOutcomeMaker.ZeroOutput.selector);
        maker.swap(address(yes), address(usd), taker, 1e11, 0, block.timestamp, "");
    }

    function test_buy_reverts_when_inventory_short() public {
        // position 0, balance 0: the maker holds nothing to deliver
        (, uint256 ask) = maker.quoteBps(address(yes));
        uint256 want = (1e6 * 10_000 * 1e18) / (ask * 1e6);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.InsufficientInventory.selector, 0, want));
        maker.getAmountOut(address(usd), address(yes), 1e6, "");
    }

    function test_transfer_failure_surfaces() public {
        // Router approves but holds no YES: the mock returns false, the maker reverts.
        address broke = address(0xB40CE);
        vm.prank(broke);
        yes.approve(address(maker), type(uint256).max);
        vm.prank(broke);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.TransferFailed.selector, address(yes)));
        maker.swap(address(yes), address(usd), taker, 1e18, 0, block.timestamp, "");
    }

    function test_reentrant_swap_is_rejected() public {
        ReentrantERC20 evil = new ReentrantERC20();
        MockERC20 yes2 = new MockERC20("YES2", 18);
        vm.startPrank(operator);
        maker.setMarket(address(yes2), address(evil), MODEL);
        maker.setQuoteParams(address(yes2), 6000, 100, 50, Q_MAX);
        yes2.mint(operator, 2e18);
        yes2.approve(address(maker), type(uint256).max);
        maker.depositOutcome(address(yes2), 2e18);
        vm.stopPrank();

        evil.mint(router, 10e6);
        vm.prank(router);
        evil.approve(address(maker), type(uint256).max);
        evil.arm(maker, address(yes2));

        vm.prank(router);
        maker.swap(address(evil), address(yes2), taker, 1e6, 0, block.timestamp, "");

        assertEq(
            evil.innerRevert(), abi.encodeWithSelector(CalibratedOutcomeMaker.Reentrancy.selector), "inner call blocked"
        );
        assertEq(yes2.balanceOf(address(0xDEAD)), 0, "nothing delivered to the reentrant recipient");
        (int256 pos,) = maker.inventoryOf(address(yes2));
        assertEq(_abs(pos), yes2.balanceOf(address(maker)), "position equals holding after the outer swap");
    }

    // ------------------------------------------------------------ inventory

    function test_inventory_limit_blocks_long_exposure() public {
        // Selling 6 outcome tokens to the maker would take position to 6 > Q_max = 5
        vm.prank(router);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.InventoryLimit.selector, 6e18, Q_MAX));
        maker.swap(address(yes), address(usd), taker, 6e18, 0, block.timestamp, "");
        // 5 is allowed
        vm.prank(router);
        maker.swap(address(yes), address(usd), taker, 5e18, 0, block.timestamp, "");
        (int256 pos, uint256 limit) = maker.inventoryOf(address(yes));
        assertEq(pos, 5e18);
        assertEq(limit, Q_MAX);
        // one more wei of exposure is refused
        vm.prank(router);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.InventoryLimit.selector, 5e18 + 1, Q_MAX));
        maker.swap(address(yes), address(usd), taker, 1, 0, block.timestamp, "");
    }

    function test_inventory_limit_blocks_short_exposure() public {
        // Tokens that arrive by direct transfer are deliverable but uncounted,
        // so buys drive position negative. The limit still binds on the short side.
        yes.mint(address(maker), 10e18);
        (, uint256 ask) = maker.quoteBps(address(yes));
        // amountIn that yields exactly 5 outcome tokens: 5 * ask / 1e4 USD
        uint256 amountIn = ask * 500;
        (uint256 out,) = maker.getAmountOut(address(usd), address(yes), amountIn, "");
        assertEq(out, 5e18);
        vm.prank(router);
        maker.swap(address(usd), address(yes), taker, amountIn, out, block.timestamp, "");
        (int256 pos,) = maker.inventoryOf(address(yes));
        assertEq(pos, -5e18);
        // one more unit of collateral would push the short past the limit
        vm.prank(router);
        vm.expectPartialRevert(CalibratedOutcomeMaker.InventoryLimit.selector);
        maker.swap(address(usd), address(yes), taker, 1e6, 0, block.timestamp, "");
    }

    function test_inventory_skews_mid_against_held_side() public {
        (uint256 bid0, uint256 ask0) = maker.quoteBps(address(yes));
        vm.prank(router);
        maker.swap(address(yes), address(usd), taker, 4e18, 0, block.timestamp, "");
        // long 4 tokens at 50 bps skew per token: mid falls 200 bps, so both sides drop
        (uint256 bid1, uint256 ask1) = maker.quoteBps(address(yes));
        assertEq(bid0 - bid1, 200);
        assertEq(ask0 - ask1, 200);
        assertEq(ask1 - bid1, FLOOR, "spread preserved");
    }

    function test_withdraw_lowers_position() public {
        _fundOutcomeInventory(3e18);
        vm.prank(operator);
        maker.withdraw(address(yes), operator, 1e18);
        (int256 pos,) = maker.inventoryOf(address(yes));
        assertEq(pos, 2e18);
        // collateral withdrawals leave position alone
        vm.prank(operator);
        maker.withdraw(address(usd), operator, 1e6);
        (pos,) = maker.inventoryOf(address(yes));
        assertEq(pos, 2e18);
    }

    // ---------------------------------------------------- round-trip safety

    function test_refuses_to_quote_when_skew_across_limit_exceeds_spread() public {
        // skew 100 bps/token over Q_max = 5 is 500 bps of excursion against a 447 bps spread
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 6000, 100, 100, Q_MAX);
        bytes memory err = abi.encodeWithSelector(CalibratedOutcomeMaker.SkewExceedsSpread.selector, 500, FLOOR);
        vm.expectRevert(err);
        maker.quoteBps(address(yes));
        vm.expectRevert(err);
        maker.getAmountOut(address(yes), address(usd), 1e18, "");
        // widening the operator spread to cover the excursion restores quoting
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 6000, 500, 100, Q_MAX);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(ask - bid, 500);
    }

    function test_round_trip_never_profits_taker() public {
        // At the boundary (skew * limit == spread) a sell-then-buy of the full
        // limit returns the taker at most what they paid.
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 6000, 500, 100, Q_MAX);
        _fundOutcomeInventory(5e18);
        vm.prank(operator);
        maker.withdraw(address(yes), operator, 5e18); // position back to 0, balance 0
        uint256 usdBefore = usd.balanceOf(router);
        vm.startPrank(router);
        uint256 got = maker.swap(address(yes), address(usd), router, 5e18, 0, block.timestamp, "");
        // buy the same five tokens back at the now-skewed ask
        (, uint256 ask1) = maker.quoteBps(address(yes));
        uint256 cost = (5e18 * ask1 * 1e6 + (10_000 * 1e18 - 1)) / (10_000 * 1e18);
        maker.swap(address(usd), address(yes), router, cost, 0, block.timestamp, "");
        vm.stopPrank();
        assertLe(usd.balanceOf(router), usdBefore, "taker cannot extract collateral by round-tripping");
        assertGe(cost, got);
    }

    // ------------------------------------------------------------- clamping

    function test_quote_clamped_at_upper_bound() public {
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 9900, 100, 0, Q_MAX);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(ask, 10_000);
        assertEq(bid, 10_000 - FLOOR);
    }

    function test_quote_clamped_at_lower_bound() public {
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 100, 100, 0, Q_MAX);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(bid, 0);
        assertEq(ask, FLOOR);
    }

    function test_quote_with_full_width_spread() public {
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 5000, 10_000, 0, Q_MAX);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(bid, 0);
        assertEq(ask, 10_000);
    }

    function test_skew_pushing_mid_below_zero_is_clamped() public {
        // forecast 1%, long 5 tokens at 50 bps each: mid would be -150
        vm.prank(operator);
        maker.setQuoteParams(address(yes), 100, 100, 50, Q_MAX);
        _fundOutcomeInventory(5e18);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(bid, 0);
        assertEq(ask, FLOOR);
    }

    // ------------------------------------------------------- improvement

    function test_improves_book_two_sided_condition() public view {
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        // A wide book centred on our mid: we tighten both sides.
        assertTrue(maker.improvesBook(address(yes), bid - 500, ask + 500));
        // A book of equal width but centred far away: our quote sits outside one side.
        assertFalse(maker.improvesBook(address(yes), ask + 10, ask + 10 + 2000));
        // A tighter book: we improve nothing.
        assertFalse(maker.improvesBook(address(yes), bid + 1, ask - 1));
        // Matching one side exactly is not an improvement: the condition is strict.
        assertFalse(maker.improvesBook(address(yes), bid, ask + 500));
        assertFalse(maker.improvesBook(address(yes), bid - 500, ask));
        assertTrue(maker.improvesBook(address(yes), bid - 1, ask + 1));
    }

    // ------------------------------------------------------------ admin

    function test_set_market_rejects_role_collisions_and_repeats() public {
        MockERC20 other = new MockERC20("NO", 18);
        vm.startPrank(operator);
        vm.expectRevert(CalibratedOutcomeMaker.MarketExists.selector);
        maker.setMarket(address(yes), address(usd), MODEL);
        vm.expectRevert(CalibratedOutcomeMaker.TokenRoleCollision.selector);
        maker.setMarket(address(other), address(other), MODEL); // self pair
        vm.expectRevert(CalibratedOutcomeMaker.TokenRoleCollision.selector);
        maker.setMarket(address(usd), address(other), MODEL); // collateral as outcome
        vm.expectRevert(CalibratedOutcomeMaker.TokenRoleCollision.selector);
        maker.setMarket(address(other), address(yes), MODEL); // outcome as collateral
        vm.stopPrank();
    }

    function test_set_market_rejects_absurd_decimals() public {
        MockERC20 weird = new MockERC20("W", 77);
        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.BadDecimals.selector, 77));
        maker.setMarket(address(weird), address(usd), MODEL);
    }

    function test_set_active_pauses_market() public {
        vm.prank(operator);
        maker.setActive(address(yes), false);
        vm.expectRevert(CalibratedOutcomeMaker.BadPair.selector);
        maker.getAmountOut(address(yes), address(usd), 1e18, "");
        vm.prank(operator);
        maker.setActive(address(yes), true);
        maker.getAmountOut(address(yes), address(usd), 1e18, "");
    }

    function test_only_operator_configures() public {
        vm.startPrank(taker);
        vm.expectRevert(CalibratedOutcomeMaker.NotOperator.selector);
        maker.setQuoteParams(address(yes), 6000, 100, 50, Q_MAX);
        vm.expectRevert(CalibratedOutcomeMaker.NotOperator.selector);
        maker.withdraw(address(usd), taker, 1);
        vm.stopPrank();
    }

    function test_constructor_rejects_zero_roles() public {
        vm.expectRevert(CalibratedOutcomeMaker.ZeroRecipient.selector);
        new CalibratedOutcomeMaker(address(0), attester, quoteSigner, MAX_AGE, MIN_N);
        vm.expectRevert(CalibratedOutcomeMaker.ZeroRecipient.selector);
        new CalibratedOutcomeMaker(operator, address(0), quoteSigner, MAX_AGE, MIN_N);
    }

    // ---------------------------------------------------------------- fuzz

    /// @dev Quote invariant over the whole parameter space: width equals
    ///      max(operator spread, floor), bounds hold, and the skew guard binds.
    function testFuzz_quote_respects_floor_and_bounds(
        uint16 forecast,
        uint16 opSpread,
        uint16 alpha,
        uint16 phi,
        uint16 skew,
        int256 position
    ) public {
        forecast = uint16(bound(forecast, 0, 10_000));
        opSpread = uint16(bound(opSpread, 0, 10_000));
        alpha = uint16(bound(alpha, 0, 10_000));
        phi = uint16(bound(phi, 0, 10_000));
        position = bound(position, -IQ_MAX, IQ_MAX);
        uint256 floor = (2 * uint256(phi) * uint256(alpha) + 9_999) / 10_000;
        vm.assume(floor <= 10_000);
        uint256 spread = opSpread > floor ? opSpread : floor;
        // keep the skew excursion within the spread so the guard does not fire
        skew = uint16(bound(skew, 0, spread / 5));

        _setRecord(alpha, phi, 15, 0, _now(), _now() - 1 days, 0);
        vm.prank(operator);
        maker.setQuoteParams(address(yes), forecast, opSpread, skew, Q_MAX);
        _setPosition(position);

        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertLe(bid, ask);
        assertLe(ask, 10_000);
        assertEq(ask - bid, spread);
        assertGe(ask - bid, maker.spreadFloorBps(address(yes)));
    }

    /// @dev Execution invariant: the taker never fills at a price better than
    ///      the posted side, and swap delivers exactly what getAmountOut quoted.
    function testFuzz_taker_never_better_than_quote(uint256 amountIn, bool buying) public {
        _fundOutcomeInventory(3e18);
        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        if (buying) {
            amountIn = bound(amountIn, 1e3, 1_500_000); // up to 1.5 USD, within inventory and limit
            (uint256 quoted,) = maker.getAmountOut(address(usd), address(yes), amountIn, "");
            vm.prank(router);
            uint256 out = maker.swap(address(usd), address(yes), taker, amountIn, quoted, block.timestamp, "");
            assertEq(out, quoted);
            // paid >= ask: amountIn / out >= ask / 1e4 in matching units
            assertGe(amountIn * 10_000 * 1e18, ask * out * 1e6);
        } else {
            amountIn = bound(amountIn, 1e14, 2e18); // at least one collateral unit out, within the limit
            (uint256 quoted,) = maker.getAmountOut(address(yes), address(usd), amountIn, "");
            vm.prank(router);
            uint256 out = maker.swap(address(yes), address(usd), taker, amountIn, quoted, block.timestamp, "");
            assertEq(out, quoted);
            // received <= bid
            assertLe(out * 10_000 * 1e18, bid * amountIn * 1e6);
        }
    }

    /// @dev After any successful swap, |position| <= limit.
    function testFuzz_position_never_exceeds_limit(int256 start, uint256 amountIn, bool buying) public {
        start = bound(start, -IQ_MAX, IQ_MAX);
        yes.mint(address(maker), 20e18); // deliverable inventory, uncounted
        _setPosition(start);
        amountIn = buying ? bound(amountIn, 1e3, 10e6) : bound(amountIn, 1e12, 10e18);
        address tin = buying ? address(usd) : address(yes);
        address tout = buying ? address(yes) : address(usd);
        vm.prank(router);
        try maker.swap(tin, tout, taker, amountIn, 0, block.timestamp, "") {
            (int256 pos, uint256 limit) = maker.inventoryOf(address(yes));
            assertLe(_abs(pos), limit);
        } catch {}
    }

    // ---------------------------------------------------------- signed quotes

    function test_signed_quote_fills_both_directions_and_matches_getAmountOut() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        // engine quotes 5900 / 6400 (spread 500 >= floor 447), one token each way
        bytes memory data = _signed(_quote(5900, 6400, 1e18, 1e18, 1));

        (uint256 quoted, bytes memory echoed) = maker.getAmountOut(address(usd), address(yes), 640_000, data);
        assertEq(keccak256(echoed), keccak256(data), "swapData is the quote data");
        assertEq(quoted, uint256(640_000) * 10_000 * 1e18 / (6400 * 1e6)); // exactly 1e18
        vm.prank(router);
        uint256 out = maker.swap(address(usd), address(yes), taker, 640_000, quoted, block.timestamp, data);
        assertEq(out, quoted);
        assertEq(yes.balanceOf(taker), 1e18);
        assertTrue(maker.nonceUsed(1));

        bytes memory sellData = _signed(_quote(5900, 6400, 1e18, 1e18, 2));
        (quoted,) = maker.getAmountOut(address(yes), address(usd), 1e18, sellData);
        assertEq(quoted, 590_000);
        vm.prank(router);
        out = maker.swap(address(yes), address(usd), taker, 1e18, quoted, block.timestamp, sellData);
        assertEq(out, 590_000);
        (int256 pos,) = maker.inventoryOf(address(yes));
        assertEq(pos, 0); // sold one, bought one back
    }

    function test_signed_quote_emits_nonce_event() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        bytes memory data = _signed(_quote(5900, 6400, 1e18, 1e18, 77));
        vm.expectEmit(true, true, false, false, address(maker));
        emit CalibratedOutcomeMaker.SignedQuoteUsed(77, address(yes));
        vm.prank(router);
        maker.swap(address(usd), address(yes), taker, 640_000, 0, block.timestamp, data);
    }

    function test_signed_quote_inside_floor_rejected_at_floor_accepted() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        // 446 wide: one bp inside the floor
        bytes memory inside = _signed(_quote(6000, 6446, 1e18, 1e18, 1));
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteInsideFloor.selector, 6000, 6446, FLOOR));
        maker.getAmountOut(address(usd), address(yes), 1e6, inside);
        vm.prank(router);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteInsideFloor.selector, 6000, 6446, FLOOR));
        maker.swap(address(usd), address(yes), taker, 1e6, 0, block.timestamp, inside);
        // 447 wide: exactly the floor
        bytes memory at = _signed(_quote(6000, 6447, 1e18, 1e18, 2));
        (uint256 quoted,) = maker.getAmountOut(address(usd), address(yes), 644_700, at);
        assertEq(quoted, 1e18);
        // the engine may quote wider than stored parameters would, never narrower
        bytes memory crossed = _signed(_quote(6500, 6400, 1e18, 1e18, 3));
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteInsideFloor.selector, 6500, 6400, FLOOR));
        maker.getAmountOut(address(usd), address(yes), 1e6, crossed);
    }

    function test_signed_quote_floor_follows_record() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        // alpha 0.354 (60 question set, as of June 2026): floor becomes 1062
        _setRecord(3540, 1500, 60, 0, _now(), _now() - 1 days, 0);
        bytes memory data = _signed(_quote(5900, 6400, 1e18, 1e18, 1));
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteInsideFloor.selector, 5900, 6400, 1062));
        maker.getAmountOut(address(usd), address(yes), 1e6, data);
        bytes memory wide = _signed(_quote(5400, 6462, 1e18, 1e18, 2));
        maker.getAmountOut(address(usd), address(yes), 646_200, wide); // buys exactly 1e18 at 6462
    }

    function test_signed_quote_requires_valid_record() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        bytes memory data = _signed(_quote(5900, 6400, 1e18, 1e18, 1));
        vm.warp(block.timestamp + MAX_AGE + 1);
        vm.expectPartialRevert(CalibratedOutcomeMaker.CalibrationStale.selector);
        maker.getAmountOut(address(usd), address(yes), 1e6, data);
    }

    function test_signed_quote_expiry_nonce_market_and_signer_checks() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0

        CalibratedOutcomeMaker.SignedQuote memory q = _quote(5900, 6400, 1e18, 1e18, 1);
        q.expiry = uint64(block.timestamp - 1);
        bytes memory expired = _signed(q);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteExpired.selector, q.expiry));
        maker.getAmountOut(address(usd), address(yes), 1e6, expired);

        bytes memory data = _signed(_quote(5900, 6400, 1e18, 1e18, 9));
        vm.prank(router);
        maker.swap(address(usd), address(yes), taker, 640_000, 0, block.timestamp, data);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.NonceUsed.selector, 9));
        maker.getAmountOut(address(usd), address(yes), 640_000, data);

        q = _quote(5900, 6400, 1e18, 1e18, 10);
        q.outcomeToken = address(usd);
        bytes memory mismatched = _signed(q);
        vm.expectRevert(
            abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteMarketMismatch.selector, address(usd), address(yes))
        );
        maker.getAmountOut(address(usd), address(yes), 1e6, mismatched);

        // signed by someone else
        q = _quote(5900, 6400, 1e18, 1e18, 11);
        (uint8 v, bytes32 r, bytes32 s_) = vm.sign(0xBAD, maker.quoteDigest(q));
        bytes memory wrong = abi.encode(q, abi.encodePacked(r, s_, v));
        address impostor = vm.addr(0xBAD);
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.BadSigner.selector, impostor));
        maker.getAmountOut(address(usd), address(yes), 1e6, wrong);

        // tampered after signing
        bytes memory good = _signed(q);
        (CalibratedOutcomeMaker.SignedQuote memory q2, bytes memory sig) =
            abi.decode(good, (CalibratedOutcomeMaker.SignedQuote, bytes));
        q2.askBps = 6401;
        bytes memory tampered = abi.encode(q2, sig);
        vm.expectPartialRevert(CalibratedOutcomeMaker.BadSigner.selector);
        maker.getAmountOut(address(usd), address(yes), 1e6, tampered);
    }

    function test_signed_quote_malformed_signatures() public {
        CalibratedOutcomeMaker.SignedQuote memory q = _quote(5900, 6400, 1e18, 1e18, 1);
        (uint8 v, bytes32 r, bytes32 s_) = vm.sign(SIGNER_PK, maker.quoteDigest(q));
        bytes memory short = abi.encode(q, new bytes(64));
        // flip s into the upper half of the curve order: malleable, refused
        bytes32 highS = bytes32(0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141 - uint256(s_));
        bytes memory malleable = abi.encode(q, abi.encodePacked(r, highS, v == 27 ? uint8(28) : uint8(27)));
        bytes memory badV = abi.encode(q, abi.encodePacked(r, s_, uint8(29)));
        vm.expectRevert(CalibratedOutcomeMaker.BadSignature.selector);
        maker.getAmountOut(address(usd), address(yes), 1e6, short);
        vm.expectRevert(CalibratedOutcomeMaker.BadSignature.selector);
        maker.getAmountOut(address(usd), address(yes), 1e6, malleable);
        vm.expectRevert(CalibratedOutcomeMaker.BadSignature.selector);
        maker.getAmountOut(address(usd), address(yes), 1e6, badV);
    }

    function test_signed_quote_size_caps_both_directions() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        // buy: cap on outcome delivered
        bytes memory data = _signed(_quote(5900, 6400, 1e18, 1e18, 1));
        // 640_001 units of USD at 6400 bps buys 1_000_001_562_500_000_000 wei of outcome
        vm.expectRevert(
            abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteSizeExceeded.selector, 1_000_001_562_500_000_000, 1e18)
        );
        maker.getAmountOut(address(usd), address(yes), 640_001, data);
        // sell: cap on outcome received
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteSizeExceeded.selector, 1e18 + 1, 1e18));
        maker.getAmountOut(address(yes), address(usd), 1e18 + 1, data);
    }

    function test_signed_quote_inventory_limit_still_enforced() public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        // engine caps are generous; the stored limit is not
        bytes memory data = _signed(_quote(5900, 6400, 100e18, 100e18, 1));
        // 3_200_001 units of USD at 6400 bps buys 5_000_001_562_500_000_000 wei, one tick over Q_max
        vm.expectRevert(
            abi.encodeWithSelector(CalibratedOutcomeMaker.InventoryLimit.selector, -5_000_001_562_500_000_000, Q_MAX)
        );
        maker.getAmountOut(address(usd), address(yes), 3_200_001, data);
        maker.getAmountOut(address(usd), address(yes), 3_200_000, data); // exactly Q_max is allowed
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.InventoryLimit.selector, IQ_MAX + 1, Q_MAX));
        maker.getAmountOut(address(yes), address(usd), Q_MAX + 1, data);
    }

    function test_signed_quote_disabled_when_no_signer() public {
        CalibratedOutcomeMaker bare = new CalibratedOutcomeMaker(operator, attester, address(0), MAX_AGE, MIN_N);
        vm.prank(operator);
        bare.setMarket(address(yes), address(usd), MODEL);
        vm.prank(attester);
        bare.setCalibration(MODEL, _record(1490, 1500, 15, 0, _now(), _now() - 1 days, 0));
        CalibratedOutcomeMaker.SignedQuote memory q = _quote(5900, 6400, 1e18, 1e18, 1);
        (uint8 v, bytes32 r, bytes32 s_) = vm.sign(SIGNER_PK, bare.quoteDigest(q));
        vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.BadSigner.selector, quoteSigner));
        bare.getAmountOut(address(usd), address(yes), 1e6, abi.encode(q, abi.encodePacked(r, s_, v)));
    }

    function test_domain_separator_binds_chain_and_contract() public view {
        bytes32 expected = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("CalibratedOutcomeMaker"),
                keccak256("1"),
                block.chainid,
                address(maker)
            )
        );
        assertEq(maker.domainSeparator(), expected);
    }

    /// @dev Over random signed spreads, the contract accepts iff the spread is at
    ///      least the floor (given the quote is otherwise valid).
    function testFuzz_signed_quote_never_inside_floor(uint16 bid, uint16 width) public {
        yes.mint(address(maker), 10e18); // deliverable inventory, position stays 0
        bid = uint16(bound(bid, 1, 9_000));
        width = uint16(bound(width, 0, 1_000));
        uint16 ask = bid + width;
        bytes memory data = _signed(_quote(bid, ask, 100e18, 100e18, 1));
        if (width < FLOOR) {
            vm.expectRevert(abi.encodeWithSelector(CalibratedOutcomeMaker.QuoteInsideFloor.selector, bid, ask, FLOOR));
            maker.getAmountOut(address(usd), address(yes), 1e5, data);
        } else {
            // 0.1 USD buys at most 1e21 / 448 wei, inside Q_max and the funded inventory
            (uint256 out,) = maker.getAmountOut(address(usd), address(yes), 1e5, data);
            assertEq(out, uint256(1e5) * 10_000 * 1e18 / (uint256(ask) * 1e6));
        }
    }

    // ------------------------------------------------------------ batch

    function test_batch_sets_many_markets() public {
        MockERC20 yes2 = new MockERC20("YES2", 18);
        vm.prank(operator);
        maker.setMarket(address(yes2), address(usd), MODEL);

        address[] memory tokens = new address[](2);
        tokens[0] = address(yes);
        tokens[1] = address(yes2);
        CalibratedOutcomeMaker.QuoteParams[] memory ps = new CalibratedOutcomeMaker.QuoteParams[](2);
        ps[0] = CalibratedOutcomeMaker.QuoteParams(7000, 600, 0, Q_MAX);
        ps[1] = CalibratedOutcomeMaker.QuoteParams(3000, 800, 0, 2 * Q_MAX);

        vm.prank(operator);
        maker.setQuoteParamsBatch(tokens, ps);

        (uint256 bid, uint256 ask) = maker.quoteBps(address(yes));
        assertEq(bid, 7000 - 300);
        assertEq(ask, 7000 + 300);
        (bid, ask) = maker.quoteBps(address(yes2));
        assertEq(bid, 3000 - 400);
        assertEq(ask, 3000 + 400);
        (, uint256 limit) = maker.inventoryOf(address(yes2));
        assertEq(limit, 2 * Q_MAX);
    }

    function test_batch_rejects_length_mismatch_unknown_market_and_non_operator() public {
        address[] memory tokens = new address[](1);
        tokens[0] = address(yes);
        CalibratedOutcomeMaker.QuoteParams[] memory ps = new CalibratedOutcomeMaker.QuoteParams[](2);
        vm.prank(operator);
        vm.expectRevert(CalibratedOutcomeMaker.LengthMismatch.selector);
        maker.setQuoteParamsBatch(tokens, ps);

        ps = new CalibratedOutcomeMaker.QuoteParams[](1);
        ps[0] = CalibratedOutcomeMaker.QuoteParams(7000, 600, 0, Q_MAX);
        tokens[0] = address(usd);
        vm.prank(operator);
        vm.expectRevert(CalibratedOutcomeMaker.UnknownMarket.selector);
        maker.setQuoteParamsBatch(tokens, ps);

        tokens[0] = address(yes);
        vm.prank(taker);
        vm.expectRevert(CalibratedOutcomeMaker.NotOperator.selector);
        maker.setQuoteParamsBatch(tokens, ps);
    }

    // ------------------------------------------------------------ helpers

    function _quote(uint16 bid, uint16 ask, uint256 maxOut, uint256 maxIn, uint256 nonce)
        internal
        view
        returns (CalibratedOutcomeMaker.SignedQuote memory)
    {
        return CalibratedOutcomeMaker.SignedQuote({
            outcomeToken: address(yes),
            bidBps: bid,
            askBps: ask,
            maxOutcomeOut: maxOut,
            maxOutcomeIn: maxIn,
            expiry: uint64(block.timestamp + 30),
            nonce: nonce
        });
    }

    /// @dev What the offchain quoting engine does: sign the EIP-712 digest and
    ///      hand the taker `abi.encode(quote, signature)`.
    function _signed(CalibratedOutcomeMaker.SignedQuote memory q) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s_) = vm.sign(SIGNER_PK, maker.quoteDigest(q));
        return abi.encode(q, abi.encodePacked(r, s_, v));
    }

    function _now() internal view returns (uint64) {
        return uint64(block.timestamp);
    }

    function _int(uint256 x) internal pure returns (int256) {
        // test amounts are far below int256.max
        // forge-lint: disable-next-line(unsafe-typecast)
        return int256(x);
    }

    function _abs(int256 x) internal pure returns (uint256) {
        // test positions are far below int256.min in magnitude
        // forge-lint: disable-next-line(unsafe-typecast)
        return x < 0 ? uint256(-x) : uint256(x);
    }

    function _record(
        uint16 alpha,
        uint16 phi,
        uint32 n,
        uint32 excluded,
        uint64 measuredAt,
        uint64 earliest,
        uint64 cutoff
    ) internal pure returns (ICalibratedOutcomeQuoter.CalibrationRecord memory) {
        return ICalibratedOutcomeQuoter.CalibrationRecord({
                alphaBps: alpha,
                phiBps: phi,
                sampleSize: n,
                excluded: excluded,
                measuredAt: measuredAt,
                earliestResolution: earliest,
                trainingCutoff: cutoff,
                datasetHash: keccak256("backtest_questions.json"),
                datasetURI: "ipfs://example"
            });
    }

    function _setRecord(
        uint16 alpha,
        uint16 phi,
        uint32 n,
        uint32 excluded,
        uint64 measuredAt,
        uint64 earliest,
        uint64 cutoff
    ) internal {
        vm.prank(attester);
        maker.setCalibration(MODEL, _record(alpha, phi, n, excluded, measuredAt, earliest, cutoff));
    }

    function _fundOutcomeInventory(uint256 amount) internal {
        vm.prank(operator);
        maker.depositOutcome(address(yes), amount);
    }

    /// @dev Drives `position` to `target` from zero: deposits for a long,
    ///      mint-then-withdraw for a short (the withdraw debits position).
    function _setPosition(int256 target) internal {
        if (target > 0) {
            _fundOutcomeInventory(_abs(target));
        } else if (target < 0) {
            uint256 amt = _abs(target);
            yes.mint(address(maker), amt);
            vm.prank(operator);
            maker.withdraw(address(yes), operator, amt);
        }
    }
}
