//! Integration tests against the compiled program in LiteSVM. Numbers are
//! the same calibration examples the Solidity suite uses: alpha 0.149 and
//! phi 0.15 (floor 447 bps), six-decimal mints, a five token inventory limit.
//! Run `anchor build` first so `target/deploy/calibrated_outcome_maker.so`
//! exists, then `cargo test`.

#![allow(clippy::result_large_err)]

use anchor_lang::{
    prelude::Pubkey,
    solana_program::{
        clock::Clock,
        instruction::{AccountMeta, Instruction},
        program_pack::Pack,
        system_instruction, system_program,
    },
    AccountDeserialize, AnchorDeserialize, InstructionData, ToAccountMetas,
};
use anchor_spl::{
    associated_token::{get_associated_token_address, spl_associated_token_account},
    token::spl_token,
};
use calibrated_outcome_maker::{
    accounts,
    error::MakerError,
    instruction as ix, math,
    state::{
        CalibrationInput, CalibrationRecord, Market, Quote, QuoteParams, SignedQuote,
        COLLATERAL_SEED, MAKER_SEED, MARKET_SEED, RECORD_SEED,
    },
};
use litesvm::{
    types::{FailedTransactionMetadata, TransactionMetadata},
    LiteSVM,
};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const DEC: u8 = 6;
const UNIT: u64 = 1_000_000;
const Q_MAX: u64 = 5 * UNIT;
const MAX_AGE: i64 = 30 * 24 * 3600;
const MIN_N: u32 = 15;
const FLOOR: u64 = 447;
const MODEL: [u8; 32] = [7u8; 32];
const FAR: i64 = i64::MAX / 2;

type TxResult = Result<TransactionMetadata, FailedTransactionMetadata>;

struct Env {
    svm: LiteSVM,
    program: Pubkey,
    operator: Keypair,
    attester: Keypair,
    quote_signer: Keypair,
    taker: Keypair,
    maker: Pubkey,
    outcome_mint: Pubkey,
    collateral_mint: Pubkey,
    market: Pubkey,
    record: Pubkey,
    outcome_vault: Pubkey,
    collateral_vault: Pubkey,
    now: i64,
}

fn code(e: MakerError) -> String {
    format!(
        "Custom({})",
        e as u32 + anchor_lang::error::ERROR_CODE_OFFSET
    )
}

fn assert_err(res: TxResult, want: MakerError) {
    match res {
        Ok(_) => panic!("expected {want:?}, transaction succeeded"),
        Err(f) => {
            let s = format!("{:?}", f.err);
            assert!(
                s.contains(&code(want)),
                "expected {want:?} ({}), got {s}\nlogs: {:#?}",
                code(want),
                f.meta.logs
            );
        }
    }
}

fn ok(res: TxResult) -> TransactionMetadata {
    match res {
        Ok(m) => m,
        Err(f) => panic!("transaction failed: {:?}\nlogs: {:#?}", f.err, f.meta.logs),
    }
}

fn record_input(now: i64) -> CalibrationInput {
    CalibrationInput {
        alpha_bps: 1490,
        phi_bps: 1500,
        sample_size: 15,
        excluded: 0,
        measured_at: now,
        earliest_resolution: now - 1,
        training_cutoff: 0,
        dataset_hash: [1u8; 32],
        dataset_uri: "ipfs://example".to_string(),
    }
}

fn params(
    forecast_bps: u16,
    operator_spread_bps: u16,
    skew_bps_per_unit: u16,
    inventory_limit: u64,
) -> QuoteParams {
    QuoteParams {
        forecast_bps,
        operator_spread_bps,
        skew_bps_per_unit,
        inventory_limit,
    }
}

impl Env {
    fn new() -> Self {
        Self::with_quote_signer(true)
    }

    /// `signed` false initialises the maker with the default key as quote
    /// signer, which disables `swap_with_quote`.
    fn with_quote_signer(signed: bool) -> Self {
        let program = calibrated_outcome_maker::id();
        let mut svm = LiteSVM::new();
        let so = include_bytes!("../../../target/deploy/calibrated_outcome_maker.so");
        svm.add_program(program, so).unwrap();

        let operator = Keypair::new();
        let attester = Keypair::new();
        let quote_signer = Keypair::new();
        let taker = Keypair::new();
        for k in [&operator, &attester, &quote_signer, &taker] {
            svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
        }
        let now = 1_770_000_000i64; // an as-of in early 2026
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = now;
        svm.set_sysvar(&clock);

        let (maker, _) =
            Pubkey::find_program_address(&[MAKER_SEED, operator.pubkey().as_ref()], &program);
        let mut env = Env {
            svm,
            program,
            operator,
            attester,
            quote_signer,
            taker,
            maker,
            outcome_mint: Pubkey::default(),
            collateral_mint: Pubkey::default(),
            market: Pubkey::default(),
            record: Pubkey::default(),
            outcome_vault: Pubkey::default(),
            collateral_vault: Pubkey::default(),
            now,
        };
        env.outcome_mint = env.create_mint(DEC);
        env.collateral_mint = env.create_mint(DEC);
        env.market = env.market_pda(env.outcome_mint);
        env.record = env.record_pda(MODEL);
        env.outcome_vault = get_associated_token_address(&env.market, &env.outcome_mint);
        env.collateral_vault = get_associated_token_address(&env.market, &env.collateral_mint);

        for who in [env.operator.pubkey(), env.taker.pubkey()] {
            for mint in [env.outcome_mint, env.collateral_mint] {
                env.create_ata(who, mint);
                env.mint_to(
                    mint,
                    get_associated_token_address(&who, &mint),
                    1_000 * UNIT,
                );
            }
        }

        let signer_key = if signed {
            env.quote_signer.pubkey()
        } else {
            Pubkey::default()
        };
        ok(env.init_maker(env.attester.pubkey(), signer_key, MAX_AGE, MIN_N));
        ok(env.set_market(env.outcome_mint, env.collateral_mint, MODEL));
        ok(env.set_quote_params(6000, 0, 0, Q_MAX));
        ok(env.set_calibration(MODEL, record_input(now)));
        env
    }

    // ------------------------------------------------------------ plumbing

    fn set_time(&mut self, t: i64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = t;
        self.svm.set_sysvar(&clock);
        self.now = t;
    }

    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> TxResult {
        self.svm.expire_blockhash();
        let bh = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&signers[0].pubkey()), &bh);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers.to_vec())
            .unwrap();
        self.svm.send_transaction(tx)
    }

    fn simulate(
        &mut self,
        ixs: &[Instruction],
        payer: &Keypair,
    ) -> Result<Vec<u8>, FailedTransactionMetadata> {
        self.svm.expire_blockhash();
        let bh = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &bh);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer]).unwrap();
        self.svm
            .simulate_transaction(tx)
            .map(|s| s.meta.return_data.data)
    }

    fn ix(
        &self,
        data: Vec<u8>,
        metas: Vec<anchor_lang::solana_program::instruction::AccountMeta>,
    ) -> Instruction {
        Instruction {
            program_id: self.program,
            accounts: metas,
            data,
        }
    }

    fn market_pda(&self, outcome_mint: Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[MARKET_SEED, self.maker.as_ref(), outcome_mint.as_ref()],
            &self.program,
        )
        .0
    }

    fn record_pda(&self, model_id: [u8; 32]) -> Pubkey {
        Pubkey::find_program_address(
            &[RECORD_SEED, self.maker.as_ref(), model_id.as_ref()],
            &self.program,
        )
        .0
    }

    fn collateral_pda(&self, mint: Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[COLLATERAL_SEED, self.maker.as_ref(), mint.as_ref()],
            &self.program,
        )
        .0
    }

    fn create_mint(&mut self, decimals: u8) -> Pubkey {
        let mint = Keypair::new();
        let rent = self
            .svm
            .minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
        let ixs = [
            system_instruction::create_account(
                &self.operator.pubkey(),
                &mint.pubkey(),
                rent,
                spl_token::state::Mint::LEN as u64,
                &spl_token::ID,
            ),
            spl_token::instruction::initialize_mint2(
                &spl_token::ID,
                &mint.pubkey(),
                &self.operator.pubkey(),
                None,
                decimals,
            )
            .unwrap(),
        ];
        let operator = self.operator.insecure_clone();
        ok(self.send(&ixs, &[&operator, &mint]));
        mint.pubkey()
    }

    fn create_ata(&mut self, owner: Pubkey, mint: Pubkey) {
        let ix = spl_associated_token_account::instruction::create_associated_token_account(
            &self.operator.pubkey(),
            &owner,
            &mint,
            &spl_token::ID,
        );
        let operator = self.operator.insecure_clone();
        ok(self.send(&[ix], &[&operator]));
    }

    fn mint_to(&mut self, mint: Pubkey, to: Pubkey, amount: u64) {
        let ix = spl_token::instruction::mint_to(
            &spl_token::ID,
            &mint,
            &to,
            &self.operator.pubkey(),
            &[],
            amount,
        )
        .unwrap();
        let operator = self.operator.insecure_clone();
        ok(self.send(&[ix], &[&operator]));
    }

    fn balance(&self, token_account: Pubkey) -> u64 {
        let acc = self.svm.get_account(&token_account).expect("token account");
        spl_token::state::Account::unpack(&acc.data).unwrap().amount
    }

    fn ata(&self, owner: Pubkey, mint: Pubkey) -> Pubkey {
        get_associated_token_address(&owner, &mint)
    }

    fn market_state(&self) -> Market {
        let acc = self.svm.get_account(&self.market).expect("market");
        Market::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn record_state(&self) -> CalibrationRecord {
        let acc = self.svm.get_account(&self.record).expect("record");
        CalibrationRecord::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    // ---------------------------------------------------------- instructions

    fn init_maker(
        &mut self,
        attester: Pubkey,
        quote_signer: Pubkey,
        max_record_age: i64,
        min_sample_size: u32,
    ) -> TxResult {
        let ix = self.ix(
            ix::InitMaker {
                attester,
                quote_signer,
                max_record_age,
                min_sample_size,
            }
            .data(),
            accounts::InitMaker {
                operator: self.operator.pubkey(),
                maker: self.maker,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        );
        let operator = self.operator.insecure_clone();
        self.send(&[ix], &[&operator])
    }

    fn set_market(
        &mut self,
        outcome_mint: Pubkey,
        collateral_mint: Pubkey,
        model_id: [u8; 32],
    ) -> TxResult {
        let market = self.market_pda(outcome_mint);
        let ix = self.ix(
            ix::SetMarket { model_id }.data(),
            accounts::SetMarket {
                operator: self.operator.pubkey(),
                maker: self.maker,
                outcome_mint,
                collateral_mint,
                market,
                collateral_marker: self.collateral_pda(collateral_mint),
                outcome_as_collateral: self.collateral_pda(outcome_mint),
                collateral_as_outcome: self.market_pda(collateral_mint),
                outcome_vault: get_associated_token_address(&market, &outcome_mint),
                collateral_vault: get_associated_token_address(&market, &collateral_mint),
                token_program: spl_token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        );
        let operator = self.operator.insecure_clone();
        self.send(&[ix], &[&operator])
    }

    fn operator_market_ix(&self, data: Vec<u8>, signer: Pubkey) -> Instruction {
        self.ix(
            data,
            accounts::OperatorMarket {
                operator: signer,
                maker: self.maker,
                market: self.market,
            }
            .to_account_metas(None),
        )
    }

    fn set_quote_params(
        &mut self,
        forecast_bps: u16,
        operator_spread_bps: u16,
        skew_bps_per_unit: u16,
        inventory_limit: u64,
    ) -> TxResult {
        let ix = self.operator_market_ix(
            ix::SetQuoteParams {
                params: params(
                    forecast_bps,
                    operator_spread_bps,
                    skew_bps_per_unit,
                    inventory_limit,
                ),
            }
            .data(),
            self.operator.pubkey(),
        );
        let operator = self.operator.insecure_clone();
        self.send(&[ix], &[&operator])
    }

    fn set_quote_params_batch_as(
        &mut self,
        signer: &Keypair,
        markets: &[Pubkey],
        params: Vec<QuoteParams>,
    ) -> TxResult {
        let mut metas = accounts::OperatorBatch {
            operator: signer.pubkey(),
            maker: self.maker,
        }
        .to_account_metas(None);
        for m in markets {
            metas.push(AccountMeta::new(*m, false));
        }
        let ix = self.ix(ix::SetQuoteParamsBatch { params }.data(), metas);
        self.send(&[ix], &[signer])
    }

    fn set_quote_params_batch(&mut self, markets: &[Pubkey], params: Vec<QuoteParams>) -> TxResult {
        let operator = self.operator.insecure_clone();
        self.set_quote_params_batch_as(&operator, markets, params)
    }

    fn set_active(&mut self, active: bool) -> TxResult {
        let ix = self.operator_market_ix(ix::SetActive { active }.data(), self.operator.pubkey());
        let operator = self.operator.insecure_clone();
        self.send(&[ix], &[&operator])
    }

    fn set_calibration_as(
        &mut self,
        signer: &Keypair,
        model_id: [u8; 32],
        record: CalibrationInput,
    ) -> TxResult {
        let ix = self.ix(
            ix::SetCalibration { model_id, record }.data(),
            accounts::SetCalibration {
                attester: signer.pubkey(),
                maker: self.maker,
                record: self.record_pda(model_id),
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        );
        self.send(&[ix], &[signer])
    }

    fn set_calibration(&mut self, model_id: [u8; 32], record: CalibrationInput) -> TxResult {
        let attester = self.attester.insecure_clone();
        self.set_calibration_as(&attester, model_id, record)
    }

    fn liquidity_ix(&self, data: Vec<u8>, mint: Pubkey, counterparty: Pubkey) -> Instruction {
        self.ix(
            data,
            accounts::Liquidity {
                operator: self.operator.pubkey(),
                maker: self.maker,
                market: self.market,
                mint,
                vault: get_associated_token_address(&self.market, &mint),
                counterparty,
                token_program: spl_token::ID,
            }
            .to_account_metas(None),
        )
    }

    fn deposit(&mut self, mint: Pubkey, amount: u64) -> TxResult {
        let from = self.ata(self.operator.pubkey(), mint);
        let ix = self.liquidity_ix(ix::Deposit { amount }.data(), mint, from);
        let operator = self.operator.insecure_clone();
        self.send(&[ix], &[&operator])
    }

    fn withdraw(&mut self, mint: Pubkey, amount: u64) -> TxResult {
        let to = self.ata(self.operator.pubkey(), mint);
        let ix = self.liquidity_ix(ix::Withdraw { amount }.data(), mint, to);
        let operator = self.operator.insecure_clone();
        self.send(&[ix], &[&operator])
    }

    /// Funds a vault directly from the mint, leaving `position` untouched,
    /// the way the Solidity tests fund the contract without a deposit.
    fn fund_vault(&mut self, mint: Pubkey, amount: u64) {
        let vault = get_associated_token_address(&self.market, &mint);
        self.mint_to(mint, vault, amount);
    }

    fn view_metas(&self) -> Vec<anchor_lang::solana_program::instruction::AccountMeta> {
        accounts::View {
            maker: self.maker,
            market: self.market,
            record: self.record,
        }
        .to_account_metas(None)
    }

    fn spread_floor(&mut self) -> Result<u64, FailedTransactionMetadata> {
        let ix = self.ix(ix::SpreadFloorBps {}.data(), self.view_metas());
        let taker = self.taker.insecure_clone();
        self.simulate(&[ix], &taker)
            .map(|d| u64::try_from_slice(&d).unwrap())
    }

    fn quote(&mut self) -> Result<(u64, u64), FailedTransactionMetadata> {
        let ix = self.ix(ix::QuoteBps {}.data(), self.view_metas());
        let taker = self.taker.insecure_clone();
        self.simulate(&[ix], &taker).map(|d| {
            let q = Quote::try_from_slice(&d).unwrap();
            (q.bid_bps, q.ask_bps)
        })
    }

    fn improves(&mut self, ref_bid: u64, ref_ask: u64) -> bool {
        let ix = self.ix(
            ix::ImprovesBook {
                ref_bid_bps: ref_bid,
                ref_ask_bps: ref_ask,
            }
            .data(),
            self.view_metas(),
        );
        let taker = self.taker.insecure_clone();
        let d = self.simulate(&[ix], &taker).map_err(|f| f.err).unwrap();
        bool::try_from_slice(&d).unwrap()
    }

    fn get_amount_out(
        &mut self,
        token_in: Pubkey,
        token_out: Pubkey,
        amount_in: u64,
    ) -> Result<u64, FailedTransactionMetadata> {
        let ix = self.ix(
            ix::GetAmountOut {
                token_in,
                token_out,
                amount_in,
            }
            .data(),
            accounts::GetAmountOut {
                maker: self.maker,
                market: self.market,
                record: self.record,
                outcome_vault: self.outcome_vault,
                collateral_vault: self.collateral_vault,
            }
            .to_account_metas(None),
        );
        let taker = self.taker.insecure_clone();
        self.simulate(&[ix], &taker)
            .map(|d| u64::try_from_slice(&d).unwrap())
    }

    fn swap_with_deadline(
        &mut self,
        token_in: Pubkey,
        token_out: Pubkey,
        amount_in: u64,
        min_out: u64,
        deadline: i64,
    ) -> TxResult {
        let ix = self.ix(
            ix::Swap {
                amount_in,
                amount_out_min: min_out,
                deadline,
            }
            .data(),
            self.swap_accounts(token_in, token_out)
                .to_account_metas(None),
        );
        let taker = self.taker.insecure_clone();
        self.send(&[ix], &[&taker])
    }

    fn swap(
        &mut self,
        token_in: Pubkey,
        token_out: Pubkey,
        amount_in: u64,
        min_out: u64,
    ) -> TxResult {
        self.swap_with_deadline(token_in, token_out, amount_in, min_out, FAR)
    }

    fn swap_accounts(&self, token_in: Pubkey, token_out: Pubkey) -> accounts::Swap {
        accounts::Swap {
            taker: self.taker.pubkey(),
            maker: self.maker,
            market: self.market,
            record: self.record,
            outcome_mint: self.outcome_mint,
            collateral_mint: self.collateral_mint,
            outcome_vault: self.outcome_vault,
            collateral_vault: self.collateral_vault,
            taker_in: self.ata(self.taker.pubkey(), token_in),
            taker_out: self.ata(self.taker.pubkey(), token_out),
            token_program: spl_token::ID,
        }
    }

    /// A quote valid for thirty seconds with no size cap.
    fn quote_at(&self, bid_bps: u16, ask_bps: u16) -> SignedQuote {
        SignedQuote {
            bid_bps,
            ask_bps,
            max_outcome_out: u64::MAX,
            max_outcome_in: u64::MAX,
            expiry: self.now + 30,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn swap_with_quote_as(
        &mut self,
        signer: &Keypair,
        token_in: Pubkey,
        token_out: Pubkey,
        amount_in: u64,
        min_out: u64,
        deadline: i64,
        quote: SignedQuote,
    ) -> TxResult {
        let ix = self.ix(
            ix::SwapWithQuote {
                quote,
                amount_in,
                amount_out_min: min_out,
                deadline,
            }
            .data(),
            accounts::SwapWithQuote {
                base: self.swap_accounts(token_in, token_out),
                quote_signer: signer.pubkey(),
            }
            .to_account_metas(None),
        );
        let taker = self.taker.insecure_clone();
        self.send(&[ix], &[&taker, signer])
    }

    fn buy_signed(&mut self, collateral_in: u64, quote: SignedQuote) -> TxResult {
        let (c, o) = (self.collateral_mint, self.outcome_mint);
        let signer = self.quote_signer.insecure_clone();
        self.swap_with_quote_as(&signer, c, o, collateral_in, 0, FAR, quote)
    }

    fn sell_signed(&mut self, outcome_in: u64, quote: SignedQuote) -> TxResult {
        let (c, o) = (self.collateral_mint, self.outcome_mint);
        let signer = self.quote_signer.insecure_clone();
        self.swap_with_quote_as(&signer, o, c, outcome_in, 0, FAR, quote)
    }

    fn buy(&mut self, collateral_in: u64) -> TxResult {
        let (c, o) = (self.collateral_mint, self.outcome_mint);
        self.swap(c, o, collateral_in, 0)
    }

    fn sell(&mut self, outcome_in: u64) -> TxResult {
        let (c, o) = (self.collateral_mint, self.outcome_mint);
        self.swap(o, c, outcome_in, 0)
    }

    fn position(&self) -> i128 {
        self.market_state().position
    }
}

// ------------------------------------------------------------------- setup

#[test]
fn setup_state_matches_configuration() {
    let env = Env::new();
    let m = env.market_state();
    assert_eq!(m.maker, env.maker);
    assert_eq!(m.outcome_mint, env.outcome_mint);
    assert_eq!(m.collateral_mint, env.collateral_mint);
    assert_eq!(m.model_id, MODEL);
    assert_eq!(
        (m.forecast_bps, m.operator_spread_bps, m.skew_bps_per_unit),
        (6000, 0, 0)
    );
    assert_eq!((m.outcome_decimals, m.collateral_decimals), (DEC, DEC));
    assert!(m.active);
    assert_eq!(m.inventory_limit, Q_MAX);
    assert_eq!(m.position, 0);
    let maker = env.svm.get_account(&env.maker).expect("maker");
    let maker = calibrated_outcome_maker::state::Maker::try_deserialize(&mut maker.data.as_slice())
        .unwrap();
    assert_eq!(maker.quote_signer, env.quote_signer.pubkey());
    let r = env.record_state();
    assert_eq!((r.alpha_bps, r.phi_bps, r.sample_size), (1490, 1500, 15));
    assert_eq!(r.dataset_uri, "ipfs://example");
    assert_eq!(env.balance(env.outcome_vault), 0);
}

#[test]
fn init_maker_rejects_zero_attester() {
    let mut env = Env::new();
    // a second maker for a fresh operator so the PDA is free
    let other = Keypair::new();
    env.svm.airdrop(&other.pubkey(), 1_000_000_000).unwrap();
    let maker =
        Pubkey::find_program_address(&[MAKER_SEED, other.pubkey().as_ref()], &env.program).0;
    let ix = env.ix(
        ix::InitMaker {
            attester: Pubkey::default(),
            quote_signer: Pubkey::default(),
            max_record_age: MAX_AGE,
            min_sample_size: MIN_N,
        }
        .data(),
        accounts::InitMaker {
            operator: other.pubkey(),
            maker,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
    );
    assert_err(env.send(&[ix], &[&other]), MakerError::ZeroAddress);
}

#[test]
fn set_market_rejects_repeat_self_pair_collisions_and_decimals() {
    let mut env = Env::new();
    // repeat: the market account already exists, so creation fails
    assert!(env
        .set_market(env.outcome_mint, env.collateral_mint, MODEL)
        .is_err());
    // self pair: the two vaults would be the same associated token account,
    // so account creation fails before the handler's own check can run
    let fresh = env.create_mint(DEC);
    assert!(env.set_market(fresh, fresh, MODEL).is_err());
    // a collateral that is already an outcome token elsewhere
    assert_err(
        env.set_market(fresh, env.outcome_mint, MODEL),
        MakerError::TokenRoleCollision,
    );
    // an outcome token that is already a collateral elsewhere
    let fresh2 = env.create_mint(DEC);
    assert_err(
        env.set_market(env.collateral_mint, fresh2, MODEL),
        MakerError::TokenRoleCollision,
    );
    // too many decimals
    let wide = env.create_mint(math::MAX_DECIMALS + 1);
    assert_err(
        env.set_market(wide, env.collateral_mint, MODEL),
        MakerError::BadDecimals,
    );
    // a second market sharing the collateral is fine
    ok(env.set_market(fresh, env.collateral_mint, MODEL));
}

#[test]
fn only_operator_and_only_attester() {
    let mut env = Env::new();
    let taker = env.taker.insecure_clone();
    let ix = env.operator_market_ix(
        ix::SetQuoteParams {
            params: params(1, 1, 0, 1),
        }
        .data(),
        taker.pubkey(),
    );
    assert_err(env.send(&[ix], &[&taker]), MakerError::NotOperator);
    let operator = env.operator.insecure_clone();
    let now = env.now;
    assert_err(
        env.set_calibration_as(&operator, MODEL, record_input(now)),
        MakerError::NotAttester,
    );
}

#[test]
fn quote_params_reject_bad_bps() {
    let mut env = Env::new();
    assert_err(
        env.set_quote_params(10_001, 0, 0, Q_MAX),
        MakerError::BadBps,
    );
    assert_err(
        env.set_quote_params(0, 10_001, 0, Q_MAX),
        MakerError::BadBps,
    );
}

// ------------------------------------------------------------- calibration

#[test]
fn calibration_rejections_and_acceptances() {
    let mut env = Env::new();
    let now = env.now;
    let mut r = record_input(now);
    r.alpha_bps = 10_001;
    assert_err(env.set_calibration(MODEL, r), MakerError::BadBps);

    let mut r = record_input(now);
    r.sample_size = MIN_N - 1;
    assert_err(env.set_calibration(MODEL, r), MakerError::SampleTooSmall);

    let mut r = record_input(now);
    r.measured_at = now + 1;
    assert_err(env.set_calibration(MODEL, r), MakerError::BadRecordTime);

    let mut r = record_input(now);
    r.earliest_resolution = now + 1;
    r.measured_at = now;
    assert_err(env.set_calibration(MODEL, r), MakerError::BadRecordTime);

    // resolution exactly at the cutoff is refused, one second after is accepted
    let mut r = record_input(now);
    r.training_cutoff = r.earliest_resolution;
    assert_err(
        env.set_calibration(MODEL, r),
        MakerError::SampleBeforeCutoff,
    );
    let mut r = record_input(now);
    r.training_cutoff = r.earliest_resolution - 1;
    ok(env.set_calibration(MODEL, r));

    let mut r = record_input(now);
    r.dataset_uri = "x".repeat(201);
    assert_err(env.set_calibration(MODEL, r), MakerError::UriTooLong);

    // replacing the record in place works
    let mut r = record_input(now);
    r.alpha_bps = 3540;
    ok(env.set_calibration(MODEL, r));
    assert_eq!(env.record_state().alpha_bps, 3540);
    assert_eq!(env.spread_floor().unwrap(), 1062);
}

#[test]
fn floor_and_quote_from_paper_numbers() {
    let mut env = Env::new();
    assert_eq!(env.spread_floor().unwrap(), FLOOR);
    assert_eq!(env.quote().unwrap(), (5777, 6224));
}

#[test]
fn floor_out_of_range_refuses_from_every_entry_point() {
    let mut env = Env::new();
    let now = env.now;
    let mut r = record_input(now);
    r.alpha_bps = 7000;
    r.phi_bps = 7500; // 2 * 0.75 * 0.70 = 1.05
    ok(env.set_calibration(MODEL, r));
    let floor_err = env.spread_floor().unwrap_err();
    assert!(format!("{:?}", floor_err.err).contains(&code(MakerError::FloorOutOfRange)));
    let quote_err = env.quote().unwrap_err();
    assert!(format!("{:?}", quote_err.err).contains(&code(MakerError::FloorOutOfRange)));
    let (c, o) = (env.collateral_mint, env.outcome_mint);
    let gao = env.get_amount_out(c, o, UNIT).unwrap_err();
    assert!(format!("{:?}", gao.err).contains(&code(MakerError::FloorOutOfRange)));
    assert_err(env.buy(UNIT), MakerError::FloorOutOfRange);
}

#[test]
fn missing_record_reports_no_calibration() {
    let mut env = Env::new();
    // a second market whose model has no record
    let other_model = [9u8; 32];
    let fresh = env.create_mint(DEC);
    ok(env.set_market(fresh, env.collateral_mint, other_model));
    env.market = env.market_pda(fresh);
    env.record = env.record_pda(other_model);
    let err = env.quote().unwrap_err();
    assert!(
        format!("{:?}", err.err).contains(&code(MakerError::NoCalibration)),
        "{:?}",
        err.err
    );
}

#[test]
fn stale_boundary_is_exact() {
    let mut env = Env::new();
    let measured = env.now;
    env.set_time(measured + MAX_AGE);
    assert_eq!(env.spread_floor().unwrap(), FLOOR);
    env.set_time(measured + MAX_AGE + 1);
    let err = env.quote().unwrap_err();
    assert!(
        format!("{:?}", err.err).contains(&code(MakerError::CalibrationStale)),
        "{:?}",
        err.err
    );
    assert_err(env.sell(UNIT), MakerError::CalibrationStale);
}

// ------------------------------------------------------------------- swaps

#[test]
fn get_amount_out_equals_swap_in_both_directions() {
    let mut env = Env::new();
    env.fund_vault(env.outcome_mint, 10 * UNIT);
    env.fund_vault(env.collateral_mint, 10 * UNIT);
    let (c, o) = (env.collateral_mint, env.outcome_mint);
    let taker = env.taker.pubkey();

    // buy with one unit of collateral
    let quoted = env.get_amount_out(c, o, UNIT).unwrap();
    assert_eq!(quoted, UNIT * 10_000 / 6224);
    let before = env.balance(env.ata(taker, o));
    ok(env.buy(UNIT));
    assert_eq!(env.balance(env.ata(taker, o)) - before, quoted);
    assert_eq!(env.position(), -(quoted as i128));

    // sell one outcome token; the quote now reflects the position (skew is 0 so unchanged)
    let quoted = env.get_amount_out(o, c, UNIT).unwrap();
    assert_eq!(quoted, UNIT * 5777 / 10_000);
    let before = env.balance(env.ata(taker, c));
    ok(env.sell(UNIT));
    assert_eq!(env.balance(env.ata(taker, c)) - before, quoted);
}

#[test]
fn swap_rejections() {
    let mut env = Env::new();
    env.fund_vault(env.outcome_mint, 10 * UNIT);
    let (c, o) = (env.collateral_mint, env.outcome_mint);

    assert_err(env.buy(0), MakerError::ZeroAmount);
    assert_err(env.swap(c, o, UNIT, u64::MAX), MakerError::Slippage);
    let past = env.now - 1;
    assert_err(
        env.swap_with_deadline(c, o, UNIT, 0, past),
        MakerError::Expired,
    );
    // selling one base unit at bid 5777 bps is worth nothing
    assert_err(env.sell(1), MakerError::ZeroOutput);
    // collateral vault is empty, so a sale cannot be paid
    assert_err(env.sell(UNIT), MakerError::InsufficientInventory);
    // inactive market
    ok(env.set_active(false));
    assert_err(env.buy(UNIT), MakerError::BadPair);
    ok(env.set_active(true));
    ok(env.buy(UNIT));
}

#[test]
fn inventory_limit_both_directions_to_the_wei() {
    let mut env = Env::new();
    env.fund_vault(env.collateral_mint, 100 * UNIT);
    env.fund_vault(env.outcome_mint, 100 * UNIT);

    // long side: taker sells exactly the limit, then one base unit more
    ok(env.sell(Q_MAX));
    assert_eq!(env.position(), Q_MAX as i128);
    assert_err(env.sell(1), MakerError::InventoryLimit);

    // unwind by withdrawing the outcome tokens the maker now holds
    ok(env.withdraw(env.outcome_mint, Q_MAX));
    assert_eq!(env.position(), 0);

    // short side: amount of collateral that buys exactly Q_MAX at ask 6224
    let exact = Q_MAX * 6224 / 10_000;
    let (c, o) = (env.collateral_mint, env.outcome_mint);
    assert_eq!(env.get_amount_out(c, o, exact).unwrap(), Q_MAX);
    ok(env.buy(exact));
    assert_eq!(env.position(), -(Q_MAX as i128));
    assert_err(env.buy(UNIT), MakerError::InventoryLimit);
}

#[test]
fn deposit_and_withdraw_move_position_for_outcome_only() {
    let mut env = Env::new();
    ok(env.deposit(env.outcome_mint, 2 * UNIT));
    assert_eq!(env.position(), 2 * UNIT as i128);
    ok(env.deposit(env.collateral_mint, 3 * UNIT));
    assert_eq!(env.position(), 2 * UNIT as i128);
    assert_eq!(env.balance(env.collateral_vault), 3 * UNIT);
    ok(env.withdraw(env.outcome_mint, UNIT));
    assert_eq!(env.position(), UNIT as i128);
    ok(env.withdraw(env.collateral_mint, 3 * UNIT));
    assert_eq!(env.position(), UNIT as i128);
    assert_eq!(env.balance(env.collateral_vault), 0);
    // a mint that is not part of the market has no vault, so the deposit fails
    let other = env.create_mint(DEC);
    env.create_ata(env.operator.pubkey(), other);
    assert!(env.deposit(other, 1).is_err());
}

#[test]
fn skew_guard_and_round_trip_cannot_profit() {
    let mut env = Env::new();
    env.fund_vault(env.collateral_mint, 100 * UNIT);
    env.fund_vault(env.outcome_mint, 100 * UNIT);

    // 500 bps per token across five tokens is 2500 > 447: refused
    ok(env.set_quote_params(6000, 0, 500, Q_MAX));
    let err = env.quote().unwrap_err();
    assert!(
        format!("{:?}", err.err).contains(&code(MakerError::SkewExceedsSpread)),
        "{:?}",
        err.err
    );
    assert_err(env.sell(UNIT), MakerError::SkewExceedsSpread);

    // 89 bps per token across five tokens is 445 <= 447: allowed
    ok(env.set_quote_params(6000, 0, 89, Q_MAX));
    assert_eq!(env.quote().unwrap(), (5777, 6224));

    // taker sells the full limit, then spends every unit of the proceeds buying back
    let taker = env.taker.pubkey();
    let (c, o) = (env.collateral_mint, env.outcome_mint);
    let col_before = env.balance(env.ata(taker, c));
    let out_before = env.balance(env.ata(taker, o));
    ok(env.sell(Q_MAX));
    let proceeds = env.balance(env.ata(taker, c)) - col_before;
    // the mid moved against the taker: bid fell by 445
    assert_eq!(env.quote().unwrap(), (5777 - 445, 6224 - 445));
    ok(env.buy(proceeds));
    assert_eq!(env.balance(env.ata(taker, c)), col_before);
    assert!(
        env.balance(env.ata(taker, o)) < out_before,
        "round trip must not return the full size"
    );
}

#[test]
fn improves_book_is_strict() {
    let mut env = Env::new();
    assert!(env.improves(5776, 6225));
    assert!(!env.improves(5777, 6225));
    assert!(!env.improves(5776, 6224));
}

// ----------------------------------------------------------- signed quotes

#[test]
fn signed_quote_fills_both_directions_at_the_signed_price() {
    let mut env = Env::new();
    env.fund_vault(env.outcome_mint, 10 * UNIT);
    env.fund_vault(env.collateral_mint, 10 * UNIT);
    let (c, o) = (env.collateral_mint, env.outcome_mint);
    let taker = env.taker.pubkey();

    // the engine quotes wider than the stored parameters would: 5500 / 6500
    let q = env.quote_at(5500, 6500);
    let before = env.balance(env.ata(taker, o));
    ok(env.buy_signed(UNIT, q));
    let got = env.balance(env.ata(taker, o)) - before;
    assert_eq!(got, UNIT * 10_000 / 6500);
    assert_eq!(env.position(), -(got as i128));

    let before = env.balance(env.ata(taker, c));
    ok(env.sell_signed(UNIT, q));
    assert_eq!(
        env.balance(env.ata(taker, c)) - before,
        UNIT * 5500 / 10_000
    );
    assert_eq!(env.position(), -(got as i128) + UNIT as i128);
}

#[test]
fn signed_quote_must_respect_the_floor() {
    let mut env = Env::new();
    env.fund_vault(env.outcome_mint, 10 * UNIT);
    env.fund_vault(env.collateral_mint, 10 * UNIT);

    // 446 bps wide: one inside the floor
    let q = env.quote_at(5777, 5777 + FLOOR as u16 - 1);
    assert_err(env.buy_signed(UNIT, q), MakerError::QuoteInsideFloor);
    // crossed
    let q = env.quote_at(6500, 5500);
    assert_err(env.buy_signed(UNIT, q), MakerError::QuoteInsideFloor);
    // ask above one
    let q = env.quote_at(9600, 10_001);
    assert_err(env.buy_signed(UNIT, q), MakerError::QuoteInsideFloor);
    // exactly at the floor is fine
    let q = env.quote_at(5777, 5777 + FLOOR as u16);
    ok(env.buy_signed(UNIT, q));
}

#[test]
fn signed_quote_floor_follows_the_record() {
    let mut env = Env::new();
    env.fund_vault(env.outcome_mint, 10 * UNIT);
    let now = env.now;
    let mut r = record_input(now);
    r.alpha_bps = 3540; // floor 1062 as of the June 2026 calibration example
    ok(env.set_calibration(MODEL, r));
    let q = env.quote_at(5000, 6000);
    assert_err(env.buy_signed(UNIT, q), MakerError::QuoteInsideFloor);
    let q = env.quote_at(5000, 6062);
    ok(env.buy_signed(UNIT, q));
}

#[test]
fn signed_quote_expiry_staleness_and_signer_checks() {
    let mut env = Env::new();
    env.fund_vault(env.outcome_mint, 10 * UNIT);
    let (c, o) = (env.collateral_mint, env.outcome_mint);

    let mut q = env.quote_at(5500, 6500);
    q.expiry = env.now - 1;
    assert_err(env.buy_signed(UNIT, q), MakerError::QuoteExpired);

    // the wrong key co-signing is refused before anything else is checked
    let q = env.quote_at(5500, 6500);
    let impostor = Keypair::new();
    env.svm.airdrop(&impostor.pubkey(), 1_000_000_000).unwrap();
    assert_err(
        env.swap_with_quote_as(&impostor, c, o, UNIT, 0, FAR, q),
        MakerError::BadSigner,
    );

    // a stale record disables signed quotes too: the floor cannot be trusted
    let measured = env.now;
    env.set_time(measured + MAX_AGE + 1);
    let q = env.quote_at(5500, 6500);
    assert_err(env.buy_signed(UNIT, q), MakerError::CalibrationStale);
}

#[test]
fn signed_quote_size_caps_and_inventory_limit() {
    let mut env = Env::new();
    env.fund_vault(env.outcome_mint, 100 * UNIT);
    env.fund_vault(env.collateral_mint, 100 * UNIT);

    // buying UNIT of collateral at 6500 yields 153_846 base units
    let out = UNIT * 10_000 / 6500;
    let mut q = env.quote_at(5500, 6500);
    q.max_outcome_out = out - 1;
    assert_err(env.buy_signed(UNIT, q), MakerError::QuoteSizeExceeded);
    q.max_outcome_out = out;
    ok(env.buy_signed(UNIT, q));

    let mut q = env.quote_at(5500, 6500);
    q.max_outcome_in = UNIT - 1;
    assert_err(env.sell_signed(UNIT, q), MakerError::QuoteSizeExceeded);
    q.max_outcome_in = UNIT;
    ok(env.sell_signed(UNIT, q));

    // the inventory limit binds regardless of what the engine signed
    let q = env.quote_at(5500, 6500);
    assert_err(env.sell_signed(Q_MAX + UNIT, q), MakerError::InventoryLimit);
}

#[test]
fn signed_quotes_disabled_when_maker_has_no_signer() {
    let mut env = Env::with_quote_signer(false);
    env.fund_vault(env.outcome_mint, 10 * UNIT);
    let q = env.quote_at(5500, 6500);
    assert_err(env.buy_signed(UNIT, q), MakerError::BadSigner);
    // the stored-parameter path is unaffected
    ok(env.buy(UNIT));
}

// ------------------------------------------------------------------- batch

#[test]
fn batch_sets_many_markets_in_one_instruction() {
    let mut env = Env::new();
    let fresh = env.create_mint(DEC);
    ok(env.set_market(fresh, env.collateral_mint, MODEL));
    let second = env.market_pda(fresh);
    let first = env.market;

    ok(env.set_quote_params_batch(
        &[first, second],
        vec![params(6100, 10, 0, 2 * UNIT), params(3000, 20, 5, 3 * UNIT)],
    ));
    let m1 = env.market_state();
    assert_eq!(
        (
            m1.forecast_bps,
            m1.operator_spread_bps,
            m1.skew_bps_per_unit,
            m1.inventory_limit
        ),
        (6100, 10, 0, 2 * UNIT)
    );
    env.market = second;
    let m2 = env.market_state();
    assert_eq!(
        (
            m2.forecast_bps,
            m2.operator_spread_bps,
            m2.skew_bps_per_unit,
            m2.inventory_limit
        ),
        (3000, 20, 5, 3 * UNIT)
    );
}

#[test]
fn batch_rejections() {
    let mut env = Env::new();
    let first = env.market;

    assert_err(
        env.set_quote_params_batch(&[first], vec![]),
        MakerError::LengthMismatch,
    );
    assert_err(
        env.set_quote_params_batch(&[first], vec![params(10_001, 0, 0, Q_MAX)]),
        MakerError::BadBps,
    );
    // an account that is not a market of this maker
    let record = env.record;
    assert!(env
        .set_quote_params_batch(&[record], vec![params(1, 0, 0, Q_MAX)])
        .is_err());
    // not the operator
    let taker = env.taker.insecure_clone();
    assert_err(
        env.set_quote_params_batch_as(&taker, &[first], vec![params(1, 0, 0, Q_MAX)]),
        MakerError::NotOperator,
    );
    // nothing changed
    let m = env.market_state();
    assert_eq!((m.forecast_bps, m.inventory_limit), (6000, Q_MAX));
}
