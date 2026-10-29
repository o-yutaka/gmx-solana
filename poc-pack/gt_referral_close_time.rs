//! PoC — GMTrade (GMX Solana) C1: referral GT bonus is bound at CLOSE time,
//! not at execution time.
//!
//! Finding (analysis: `~/workspace/bugbounty/gmtrade-c1-pricing.md`,
//! mechanism: `redteam-gmtrade-composition.md` §C1):
//!
//! * At order execution, `Order::unchecked_process_gt`
//!   (`programs/store/src/states/order.rs`) converts the order's paid fee
//!   value into GT minted to the trader and records the amount on the
//!   order as `order.gt_reward`. The referral *discount* leg is also
//!   evaluated at execution, from the referrer of record at that moment.
//! * At order close, `CloseOrderV2::process_gt_reward` →
//!   `mint_gt_reward_for_referrer`
//!   (`programs/store/src/instructions/exchange/order.rs`) reads the
//!   referrer from the trader's User account **at close** and the factor
//!   from that referrer's rank **at close**, and mints
//!   `gt_reward × factor` of fresh GT (a ledger mint in `GtState::mint_to`)
//!   to whoever is the referrer *then*. There is no referrer / rank /
//!   factor snapshot on the Order account.
//!
//! Consequences exercised by the tests below:
//!
//! * Case 1 (diversion — the High claim, "Theft of unclaimed yield"):
//!   trader U executes while referred by genuine referrer X, then calls
//!   `set_referrer` to their own alt B, then closes. B receives
//!   `gt_reward × factor(rank_B)`; X — whose referral the protocol itself
//!   used at execution for the discount leg — receives 0.
//! * Case 2 (control): identical flow without the switch. X receives the
//!   bonus. (By construction the *amount* equals Case 1's when X and B
//!   are both rank 0; the diversion is proven by the payee deltas, not
//!   by a total difference. See README-c1.md.)
//! * Case 3 (creation from nothing): U executes with **no** referrer,
//!   sets referrer = B, closes. B receives a bonus that, per Case 4,
//!   would otherwise never be minted at all.
//! * Case 4 (control): U executes unreferred and closes unreferred.
//!   `mint_gt_reward_for_referrer` early-returns on `referrer == None`;
//!   zero GT is minted at close.
//!
//! Placement: `programs/store/tests/gt_referral_close_time.rs` inside a
//! checkout of `gmsol-labs/gmx-solana` at the analysed ref
//! `a10a6599a42442aac2b4873fb058616a40e07f27`. Framework:
//! `solana-program-test`, consistent with the sibling freeze PoC in this
//! directory. Instructions are built with the program crate's generated
//! client (`gmsol_store::accounts::*` / `gmsol_store::instruction::*` +
//! `ToAccountMetas` / `InstructionData`) — the pattern used by the repo's
//! own `programs/treasury/tests/complete_gt_exchange.rs`.
//!
//! STATUS: written as a finished, ready-to-run asset; NOT YET EXECUTED
//! (authored on a machine with no Rust/Solana toolchain). Every point
//! that could not be verified from source in this environment is marked
//! `TODO-VERIFY` rather than silently guessed. See README-c1.md for the
//! run commands and the full TODO-VERIFY checklist.
//!
//! No mainnet interaction; everything runs against a local ProgramTest
//! ledger. Nothing has been submitted anywhere.

use anchor_lang::{
    solana_program::{
        instruction::Instruction, native_token::LAMPORTS_PER_SOL, pubkey::Pubkey,
    },
    InstructionData, ToAccountMetas,
};
use solana_program_test::{tokio, BanksClient, ProgramTest};
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::Transaction,
};

// ===========================================================================
// Deployed-config fixture (mainnet-verified 2026-10-29, see pricing doc §2)
// ===========================================================================

/// GT decimals on mainnet.
pub const GT_DECIMALS: u8 = 7;
/// One whole GT in base units.
pub const GT_UNIT: u64 = 10u64.pow(GT_DECIMALS as u32);
/// USD values / factors are scaled by 10^20 (`MARKET_DECIMALS = 20`,
/// `crates/programs/src/constants/mod.rs`; store program re-exports the
/// same constant from `programs/store/src/constants/mod.rs`).
pub const MARKET_USD_UNIT: u128 = 10u128.pow(20);
/// Deployed initial minting cost: $0.01 per GT. In code units the cost is
/// `price × 10^(MARKET_DECIMALS − GT_DECIMALS)`, i.e.
/// `MARKET_USD_UNIT / 100 / GT_UNIT` — the same expression the repo's own
/// `tests/anchor_test/setup.rs::Deployment::initialize_gt` uses.
pub const INITIAL_MINTING_COST: u128 = MARKET_USD_UNIT / 100 / GT_UNIT as u128;
/// Deployed minting-cost growth: ×1.021 per grow step.
pub const MINTING_COST_GROW_FACTOR: u128 = MARKET_USD_UNIT * 1021 / 1000;
/// Deployed grow step: cost steps every 210,000 GT minted.
pub const GROW_STEP_GT: u64 = 210_000 * GT_UNIT;
/// Deployed rank thresholds (referrer GT balance, whole GT). Rank of a
/// fresh alt holding 0 GT is 0 (`GtState::unchecked_update_rank` binary
/// search: `Err(0) → rank 0`).
pub const RANK_THRESHOLDS_WHOLE_GT: [u64; 9] = [
    600, 2_000, 6_000, 20_000, 60_000, 200_000, 600_000, 2_000_000, 6_000_000,
];
/// Deployed referral reward factors, rank 0..=9 (pricing doc §2):
/// 0.50, 0.60, 0.65, 0.70, 0.75, 0.80, 0.85, 0.90, 0.95, 1.00.
/// NOTE: unlike the order-fee discount factors, the on-chain setter for
/// this table has **no ≤ 100% cap** — 1.00 at rank 9 is an admin choice.
pub const REFERRAL_REWARD_FACTORS: [u128; 10] = [
    MARKET_USD_UNIT * 50 / 100,
    MARKET_USD_UNIT * 60 / 100,
    MARKET_USD_UNIT * 65 / 100,
    MARKET_USD_UNIT * 70 / 100,
    MARKET_USD_UNIT * 75 / 100,
    MARKET_USD_UNIT * 80 / 100,
    MARKET_USD_UNIT * 85 / 100,
    MARKET_USD_UNIT * 90 / 100,
    MARKET_USD_UNIT * 95 / 100,
    MARKET_USD_UNIT,
];
/// Rank-0 referral factor (0.50) — X and B both start at rank 0, so every
/// case below asserts against this single factor. No rank farming needed.
pub const FACTOR_RANK_0: u128 = REFERRAL_REWARD_FACTORS[0];
/// Deployed order-fee discount factors, rank 0..=9: 0%, 2%, …, 10%.
/// Not load-bearing for any assertion (the discount leg is not measured);
/// included so the executed order's fee path matches production shape.
/// TODO-VERIFY: the pricing doc verified only the endpoints and the
/// "0%, 2%, 3%, …, 10%" summary; the intermediate values below are the
/// natural monotone reading. Replace with the exact deployed table if a
/// re-read of the live Store account disagrees.
pub const ORDER_FEE_DISCOUNT_FACTORS: [u128; 10] = [
    0,
    MARKET_USD_UNIT * 2 / 100,
    MARKET_USD_UNIT * 3 / 100,
    MARKET_USD_UNIT * 4 / 100,
    MARKET_USD_UNIT * 5 / 100,
    MARKET_USD_UNIT * 6 / 100,
    MARKET_USD_UNIT * 7 / 100,
    MARKET_USD_UNIT * 8 / 100,
    MARKET_USD_UNIT * 9 / 100,
    MARKET_USD_UNIT * 10 / 100,
];
/// Global "order fee discount for referred user" factor (10%), mirroring
/// `tests/anchor_test/setup.rs` (`FactorKey::OrderFeeDiscountForReferredUser`).
pub const REFERRED_USER_DISCOUNT_FACTOR: u128 = MARKET_USD_UNIT * 10 / 100;

/// Referral codes (`ReferralCodeBytes = [u8; 8]`, see the
/// `#[instruction(code: [u8; 8])]` attribute on `SetReferrer`).
pub const CODE_X: [u8; 8] = *b"REFERRRX";
pub const CODE_B: [u8; 8] = *b"REFERRRB";

// ===========================================================================
// Exact formula mirrors (assertions are computed with these, never hardcoded)
// ===========================================================================

/// Mirror of `GtState::get_mint_amount` + `Order::unchecked_process_gt`:
///
/// ```text
/// value_to_mint_for = paid_fee_value_after − minted_fee_value_before
/// minted            = value_to_mint_for / minting_cost        (floor)
/// ```
///
/// (`get_mint_amount` returns `(minted, minted_value = value − value %
/// minting_cost, minting_cost)`; the remainder is carried in the user's
/// `minted_fee_value` and is *not* part of this order's `gt_reward`.)
/// Valid while no grow step is crossed mid-order — the fixture's grow
/// step is 210,000 GT and a single order mints far less; the caller
/// asserts `minting_cost` is unchanged across execution.
pub fn expected_gt_reward(
    paid_fee_value_before: u128,
    minted_fee_value_before: u128,
    paid_fee_value_after: u128,
    minting_cost: u128,
) -> u64 {
    debug_assert!(paid_fee_value_after >= paid_fee_value_before);
    let _ = paid_fee_value_before;
    let value_to_mint_for = paid_fee_value_after - minted_fee_value_before;
    (value_to_mint_for / minting_cost) as u64
}

/// Mirror of the bonus computation in `mint_gt_reward_for_referrer`:
///
/// ```text
/// reward = apply_factor::<MARKET_DECIMALS>(gt_reward, factor)
///        = gt_reward × factor / 10^20                          (floor)
/// ```
///
/// TODO-VERIFY: `gmsol_model::utils::apply_factor` rounding for unsigned
/// values (expected: floor via mul-div). If it rounds differently, this
/// helper is the single place to fix.
pub fn expected_referral_bonus(gt_reward: u64, factor: u128) -> u64 {
    ((gt_reward as u128 * factor) / MARKET_USD_UNIT) as u64
}

// ===========================================================================
// Low-level helpers
// ===========================================================================

/// Build an `Instruction` from a generated accounts struct + args struct,
/// exactly as in the repo's own program-test-style unit tests.
macro_rules! ix {
    ($accounts:expr, $args:expr) => {
        Instruction {
            program_id: gmsol_store::ID,
            accounts: $accounts.to_account_metas(None),
            data: $args.data(),
        }
    };
}
pub(crate) use ix;

/// Send a single-instruction transaction signed by `payer` (+ extra signers).
pub async fn send(
    client: &mut BanksClient,
    payer: &Keypair,
    extra_signers: &[&Keypair],
    instructions: Vec<Instruction>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra_signers);
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &signers,
        client.get_latest_blockhash().await?,
    );
    client.process_transaction(tx).await?;
    Ok(())
}

/// Load a zero-copy Anchor account (`Store`, `UserHeader`, `Order`, …)
/// from the ProgramTest ledger.
///
/// TODO-VERIFY: the exact off-chain load API for the workspace's Anchor
/// version (0.31.x per `Anchor.toml`): expected
/// `anchor_lang::ZeroCopy::load(&account.data[8..])` after checking the
/// 8-byte discriminator; if that trait method is not exposed, fall back
/// to `bytemuck::from_bytes::<T>(&account.data[8..8 + size_of::<T>()])`.
pub async fn load_zero_copy<T>(
    client: &mut BanksClient,
    address: &Pubkey,
) -> Result<T, Box<dyn std::error::Error>>
where
    T: anchor_lang::ZeroCopy + Clone,
{
    let account = client
        .get_account(*address)
        .await?
        .ok_or("account not found")?;
    let data = account.data;
    // TODO-VERIFY: discriminator check (`T::DISCRIMINATOR`) and load call.
    let value = T::load(&data[8..])?;
    Ok(value.clone())
}

// TODO-VERIFY (one grep at build time resolves every seed below): the
// `Seed` constants live with the state types
// (`programs/store/src/states/*.rs`, re-exported through `gmsol_utils`).
// The seed *composition* is copied verbatim from the Anchor constraints
// in the instruction sources; only the literal byte values of the named
// seeds need confirming:
//   UserHeader::SEED        — seeds = [SEED, store, owner]
//   ReferralCodeV2::SEED    — seeds = [SEED, store, code]
//   Order::SEED             — seeds = [SEED, store, owner, nonce]
//   Position::SEED          — seeds = [SEED, store, owner, market_token_mint,
//                                    collateral_token, kind_byte]
//   Store::WALLET_SEED      — seeds = [WALLET_SEED, store]
//   MARKET_VAULT_SEED       — seeds = [MARKET_VAULT_SEED, store, mint]
//   CallbackAuthority       — seeds = [CALLBACK_AUTHORITY_SEED] (global)
pub mod seeds {
    pub const USER: &[u8] = b"user"; // TODO-VERIFY
    pub const REFERRAL_CODE: &[u8] = b"referral_code"; // TODO-VERIFY
    pub const ORDER: &[u8] = b"order"; // TODO-VERIFY
    pub const POSITION: &[u8] = b"position"; // TODO-VERIFY
    pub const STORE_WALLET: &[u8] = b"store_wallet"; // TODO-VERIFY
    pub const MARKET_VAULT: &[u8] = b"market_vault"; // TODO-VERIFY
    pub const CALLBACK_AUTHORITY: &[u8] = b"callback_authority"; // TODO-VERIFY
}

pub fn user_pda(store: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[seeds::USER, store.as_ref(), owner.as_ref()],
        &gmsol_store::ID,
    )
    .0
}

pub fn referral_code_pda(store: &Pubkey, code: &[u8; 8]) -> Pubkey {
    Pubkey::find_program_address(
        &[seeds::REFERRAL_CODE, store.as_ref(), code],
        &gmsol_store::ID,
    )
    .0
}

pub fn order_pda(store: &Pubkey, owner: &Pubkey, nonce: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(
        &[seeds::ORDER, store.as_ref(), owner.as_ref(), nonce],
        &gmsol_store::ID,
    )
    .0
}

pub fn position_pda(
    store: &Pubkey,
    owner: &Pubkey,
    market_token_mint: &Pubkey,
    collateral_token: &Pubkey,
    kind_byte: u8,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            seeds::POSITION,
            store.as_ref(),
            owner.as_ref(),
            market_token_mint.as_ref(),
            collateral_token.as_ref(),
            &[kind_byte],
        ],
        &gmsol_store::ID,
    )
    .0
}

pub fn store_wallet_pda(store: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seeds::STORE_WALLET, store.as_ref()], &gmsol_store::ID).0
}

pub fn market_vault_pda(store: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[seeds::MARKET_VAULT, store.as_ref(), mint.as_ref()],
        &gmsol_store::ID,
    )
    .0
}

pub fn callback_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[seeds::CALLBACK_AUTHORITY], &gmsol_store::ID).0
}

/// GT ledger snapshot of one user, read from their User account.
/// Field accessors mirror the in-crate usage
/// (`user.gt.amount`, `user.gt.paid_fee_value()`, `user.gt.rank`, …).
///
/// TODO-VERIFY: import path and accessor names for the shared user type
/// (candidates: `gmsol_utils::user::UserHeader` — the store program's
/// `states::user` re-export — with public `gt` / `referral` accessors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GtSnapshot {
    pub amount: u64,
    pub rank: u8,
    pub paid_fee_value: u128,
    pub minted_fee_value: u128,
}

pub async fn gt_snapshot(
    client: &mut BanksClient,
    store: &Pubkey,
    owner: &Pubkey,
) -> Result<GtSnapshot, Box<dyn std::error::Error>> {
    // TODO-VERIFY: type path (see note above).
    let user = load_zero_copy::<gmsol_store::states::UserHeader>(client, &user_pda(store, owner))
        .await?;
    Ok(GtSnapshot {
        amount: user.gt.amount,
        rank: user.gt.rank,
        paid_fee_value: user.gt.paid_fee_value(),
        minted_fee_value: user.gt.minted_fee_value(),
    })
}

/// `(gt supply, total_minted, minting_cost)` from the Store account.
/// `Store::gt()` and the `GtState` getters are public in the source.
pub async fn gt_globals(
    client: &mut BanksClient,
    store: &Pubkey,
) -> Result<(u64, u64, u128), Box<dyn std::error::Error>> {
    let store_state = load_zero_copy::<gmsol_store::states::Store>(client, store).await?;
    let gt = store_state.gt();
    Ok((gt.supply(), gt.total_minted(), gt.minting_cost()))
}

// ===========================================================================
// Fixture
// ===========================================================================
//
// Setup mirrors the repo's own test deployment
// (`tests/anchor_test/setup.rs::Deployment`) in shape — admin + keeper
// with roles, GT initialised by the keeper, users with referral codes —
// but drives everything through `solana-program-test` and the generated
// client instead of the RPC `Deployment` harness.
//
// TODO-VERIFY (fixture seams): the generated-client field lists for the
// setup instructions (`initialize`, role grants, token map / oracle /
// token config, market creation, user initialisation) must be confirmed
// against `crates/programs` (the generated client source) or the SDK
// builders in `crates/sdk/src/client/ops/*` at build time. Each helper
// below names the SDK op it mirrors. The *lifecycle* instructions used
// by the tests themselves (referral code, set_referrer, create / execute
// / close order) are constructed from field lists read directly from
// the program source.

pub struct Fixture {
    pub store: Pubkey,
    pub admin: Keypair,
    pub keeper: Keypair,
    /// Trader "U" — the referee whose referrer is switched.
    pub trader: Keypair,
    /// Genuine referrer "X".
    pub referrer_x: Keypair,
    /// Attacker alt "B" (also controlled by the trader in the narrative;
    /// a distinct keypair here, as on-chain).
    pub referrer_b: Keypair,
    pub token_map: Pubkey,
    pub oracle: Pubkey,
    pub market: Pubkey,
    pub market_token_mint: Pubkey,
    /// Long token = index token = collateral token for the test orders.
    pub long_token: Pubkey,
    /// Short token (stable).
    pub short_token: Pubkey,
    /// Price-feed accounts for [long, short], in the order the execute
    /// instruction expects them as remaining accounts.
    /// TODO-VERIFY: exact feed-account derivation for the mock oracle
    /// (see `setup_oracle_and_tokens`).
    pub feed_accounts: Vec<Pubkey>,
}

pub const ROLE_ORDER_KEEPER: &str = "ORDER_KEEPER"; // TODO-VERIFY RoleKey encoding in grant args
pub const ROLE_GT_CONTROLLER: &str = "GT_CONTROLLER"; // TODO-VERIFY
pub const ROLE_MARKET_KEEPER: &str = "MARKET_KEEPER"; // TODO-VERIFY

pub async fn setup_fixture(
    ctx: &mut solana_program_test::ProgramTestContext,
) -> Result<Fixture, Box<dyn std::error::Error>> {
    let payer = ctx.payer.insecure_clone();
    let admin = Keypair::new();
    let keeper = Keypair::new();
    let trader = Keypair::new();
    let referrer_x = Keypair::new();
    let referrer_b = Keypair::new();

    // Fund actors from the test payer.
    for kp in [&admin, &keeper, &trader, &referrer_x, &referrer_b] {
        send(
            &mut ctx.banks_client,
            &payer,
            &[],
            vec![solana_sdk::system_instruction::transfer(
                &payer.pubkey(),
                &kp.pubkey(),
                10 * LAMPORTS_PER_SOL,
            )],
        )
        .await?;
    }

    // --- Store ------------------------------------------------------------
    // SDK mirror: `StoreOps::initialize`. The store account key is chosen
    // by the creator (the SDK generates it); all other accounts derive
    // from it.
    // TODO-VERIFY: generated `accounts::Initialize` / `instruction::Initialize`
    // field lists and whether the store keypair must sign.
    let store_keypair = Keypair::new();
    let store = store_keypair.pubkey();
    initialize_store(ctx, &admin, &store_keypair).await?;

    // --- Roles --------------------------------------------------------------
    // Keeper executes orders (ORDER_KEEPER), configures GT
    // (GT_CONTROLLER: factor setters are `only GT_CONTROLLER` per
    // `programs/store/src/instructions/gt.rs`) and sets the discount
    // factor table (MARKET_KEEPER).
    // SDK mirror: `RoleOps::grant_role` / `enable_role`.
    // TODO-VERIFY: generated instruction names + how `RoleKey` is encoded
    // in args (enum vs. string) — see `crates/programs` client and
    // `gmsol_utils::role::RoleKey`.
    for role in [ROLE_ORDER_KEEPER, ROLE_GT_CONTROLLER, ROLE_MARKET_KEEPER] {
        grant_role(ctx, &admin, &store, &keeper.pubkey(), role).await?;
    }

    // --- Token map, oracle, tokens, market ---------------------------------
    // SDK mirrors: `TokenConfigOps` (token map + configs),
    // `OracleOps` (oracle init), `MarketOps::create_market`, and the
    // pull-oracle mock used by `tests/anchor_test`
    // (`ChainlinkPullOracleFactory` + the mock verifier program listed in
    // `Anchor.toml` `[programs.localnet]` as
    // `gmsol_mock_chainlink_verifier`).
    // TODO-VERIFY: registering the mock verifier `.so`
    // (`external-programs/` in the repo) with
    // `ProgramTest::add_program`, the feed-account derivation, and how
    // prices are posted to the mock (anchor_test posts via its oracle
    // factory before each execution).
    let (token_map, oracle, long_token, short_token, feed_accounts) =
        setup_oracle_and_tokens(ctx, &admin, &keeper, &store).await?;
    let (market, market_token_mint) =
        create_market(ctx, &keeper, &store, &long_token, &short_token).await?;

    // --- GT ------------------------------------------------------------------
    // Keeper-configured, with the DEPLOYED parameter table (§ constants).
    // SDK mirrors (`crates/sdk` `GtOps` + `ConfigOps`), exactly as called
    // by `tests/anchor_test/setup.rs::Deployment::initialize_gt`:
    //   initialize_gt(store, decimals, initial_minting_cost, grow_factor,
    //                 grow_step, ranks)
    //   insert_global_factor_by_key(store, OrderFeeDiscountForReferredUser, 10%)
    //   gt_set_order_fee_discount_factors(store, [...])
    //   gt_set_referral_reward_factors(store, [...])
    // TODO-VERIFY: generated `accounts::ConfigureGt` fields
    // (expected `{ authority, store }`) and arg types.
    initialize_gt(ctx, &keeper, &store).await?;

    // --- Users ---------------------------------------------------------------
    // SDK mirror: user initialisation (`initialize_user` in the store
    // program; cf. the freeze PoC design's `prepare_user` note).
    // TODO-VERIFY: generated instruction name / accounts
    // (expected `{ owner, store, user, system_program }`).
    for kp in [&trader, &referrer_x, &referrer_b] {
        initialize_user(ctx, kp, &store).await?;
    }

    // --- Referral codes --------------------------------------------------------
    // X and B each register a referral code (creates the ReferralCodeV2
    // PDA and records it on their User account). Account list read from
    // `programs/store/src/instructions/user.rs::InitializeReferralCode`.
    // TODO-VERIFY: the first two generated fields (expected
    // `owner`, `store`, in source order) — the tail of the struct
    // (`referral_code`, `user`, `system_program`) is source-verified.
    initialize_referral_code(ctx, &referrer_x, &store, CODE_X).await?;
    initialize_referral_code(ctx, &referrer_b, &store, CODE_B).await?;

    // --- Trader collateral -----------------------------------------------------
    // Mint long-token collateral to the trader's ATA. The market's vaults
    // are seeded with a small amount of both tokens so execution has
    // liquidity to draw on.
    // TODO-VERIFY: whether `create_market` already initialises the market
    // vault token accounts (expected: yes, as part of market creation);
    // the ATA/mint helpers below use plain SPL token instructions.
    fund_collateral(ctx, &payer, &trader.pubkey(), &long_token, COLLATERAL_AMOUNT).await?;
    seed_market_vaults(ctx, &payer, &store, &long_token, &short_token).await?;

    Ok(Fixture {
        store,
        admin,
        keeper,
        trader,
        referrer_x,
        referrer_b,
        token_map,
        oracle,
        market,
        market_token_mint,
        long_token,
        short_token,
        feed_accounts,
    })
}

/// Collateral per order: 500 long tokens (9 decimals → 500 × 10^9).
/// At the mock price of $100 that is $50,000 of collateral against a
/// $20,000 size — fees are whatever the market config charges; no
/// assertion depends on the fee amount, only on `paid_fee_value > 0`.
pub const COLLATERAL_AMOUNT: u64 = 500_000_000_000;
/// Order size delta: $20,000 in USD-value units (10^20 scale).
pub const SIZE_DELTA_VALUE: u128 = 20_000 * MARKET_USD_UNIT;

// ---------------------------------------------------------------------------
// Setup helpers (fixture seams — see TODO-VERIFY notes)
// ---------------------------------------------------------------------------

pub async fn initialize_store(
    ctx: &mut solana_program_test::ProgramTestContext,
    admin: &Keypair,
    store_keypair: &Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO-VERIFY: `accounts::Initialize` fields and `instruction::Initialize`
    // args (SDK: `StoreOps::initialize(store, authority)`; the store
    // account is creator-chosen and signs).
    let accounts = gmsol_store::accounts::Initialize {
        payer: admin.pubkey(),
        authority: admin.pubkey(),
        store: store_keypair.pubkey(),
        system_program: solana_sdk::system_program::ID,
    };
    let args = gmsol_store::instruction::Initialize {};
    send(
        &mut ctx.banks_client,
        admin,
        &[store_keypair],
        vec![ix!(accounts, args)],
    )
    .await
}

pub async fn grant_role(
    ctx: &mut solana_program_test::ProgramTestContext,
    admin: &Keypair,
    store: &Pubkey,
    user: &Pubkey,
    role: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO-VERIFY: instruction pair is `enable_role` + `grant_role` in
    // `programs/store/src/instructions/roles.rs`; arg encoding of the
    // role (`RoleKey`) in the generated client. The store admin
    // (`authority`) is the signer.
    let _ = (ctx, admin, store, user, role);
    Err("TODO-VERIFY: wire grant_role to the generated roles client (see comment)".into())
}

pub async fn setup_oracle_and_tokens(
    ctx: &mut solana_program_test::ProgramTestContext,
    admin: &Keypair,
    keeper: &Keypair,
    store: &Pubkey,
) -> Result<(Pubkey, Pubkey, Pubkey, Pubkey, Vec<Pubkey>), Box<dyn std::error::Error>> {
    // TODO-VERIFY (largest fixture seam — resolve from
    // `tests/anchor_test/setup.rs` + `crates/sdk/src/client/ops`):
    //   1. `initialize_token_map` (ConfigOps/TokenConfigOps) → token_map PDA.
    //   2. `initialize_oracle` (OracleOps) → oracle PDA.
    //   3. Create the long/short SPL mints (plain SPL instructions;
    //      decimals 9 / 6) and `insert_token_config` for each with a
    //      Chainlink feed id (TokenConfigOps + `UpdateFeedConfig`).
    //   4. Register the mock Chainlink verifier program
    //      (`gmsol_mock_chainlink_verifier`, see `Anchor.toml`) with the
    //      ProgramTest instance *before* `start_with_context` — i.e.
    //      this step may need to move into the test's ProgramTest
    //      construction; anchor_test instead loads
    //      `external-programs/chainlink-store.so` as genesis.
    //   5. Post mock prices: long = $100, short = $1 (the anchor_test
    //      oracle factory does the equivalent before each execution;
    //      `post_mock_prices` below is called again before execute).
    let _ = (ctx, admin, keeper, store);
    Err("TODO-VERIFY: wire token map / oracle / token configs (see comment)".into())
}

/// Post fresh mock prices for the fixture tokens. Called once in setup
/// and again immediately before each order execution (price freshness
/// is validated against the execute instruction's `recent_timestamp`).
pub async fn post_mock_prices(
    ctx: &mut solana_program_test::ProgramTestContext,
    fx: &Fixture,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO-VERIFY: mock-oracle price posting route (see
    // `setup_oracle_and_tokens` note 5). Prices: long $100, short $1,
    // in the mock feed's own decimal format.
    let _ = (ctx, fx);
    Err("TODO-VERIFY: wire mock price posting (see comment)".into())
}

pub async fn create_market(
    ctx: &mut solana_program_test::ProgramTestContext,
    keeper: &Keypair,
    store: &Pubkey,
    long_token: &Pubkey,
    short_token: &Pubkey,
) -> Result<(Pubkey, Pubkey), Box<dyn std::error::Error>> {
    // TODO-VERIFY: `MarketOps::create_market(store, index_token,
    // long_token, short_token)` → generated accounts/args; the market
    // account and its market-token mint are PDAs derived from the index
    // token (see `gmsol_utils::market` address helpers). Default market
    // config from creation is used as-is (fee factors are not asserted).
    let _ = (ctx, keeper, store, long_token, short_token);
    Err("TODO-VERIFY: wire create_market to the generated market client (see comment)".into())
}

pub async fn initialize_gt(
    ctx: &mut solana_program_test::ProgramTestContext,
    keeper: &Keypair,
    store: &Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    // All four calls use the `ConfigureGt` accounts pattern
    // (`programs/store/src/instructions/gt.rs`); the keeper holds
    // GT_CONTROLLER (and MARKET_KEEPER for the discount table).
    // TODO-VERIFY: `accounts::ConfigureGt` field list
    // (expected `{ authority, store }`) and the generated arg struct
    // field names/types (`ranks: Vec<u64>`, `factors: Vec<u128>`).
    let accounts = || gmsol_store::accounts::ConfigureGt {
        authority: keeper.pubkey(),
        store: *store,
    };
    let ranks: Vec<u64> = RANK_THRESHOLDS_WHOLE_GT
        .iter()
        .map(|whole| whole * GT_UNIT)
        .collect();
    send(
        &mut ctx.banks_client,
        keeper,
        &[],
        vec![
            ix!(
                accounts(),
                gmsol_store::instruction::InitializeGt {
                    decimals: GT_DECIMALS,
                    initial_minting_cost: INITIAL_MINTING_COST,
                    minting_cost_grow_factor: MINTING_COST_GROW_FACTOR,
                    grow_step: GROW_STEP_GT,
                    ranks,
                }
            ),
            // SDK mirror: `gt_set_order_fee_discount_factors`.
            ix!(
                accounts(),
                gmsol_store::instruction::GtSetOrderFeeDiscountFactors {
                    factors: ORDER_FEE_DISCOUNT_FACTORS.to_vec(),
                }
            ),
            // SDK mirror: `gt_set_referral_reward_factors` — THE table
            // under test (deployed values, rank 0 = 0.50).
            ix!(
                accounts(),
                gmsol_store::instruction::GtSetReferralRewardFactors {
                    factors: REFERRAL_REWARD_FACTORS.to_vec(),
                }
            ),
        ],
    )
    .await?;
    // Global referred-user discount factor (10%), mirroring setup.rs's
    // `insert_global_factor_by_key(OrderFeeDiscountForReferredUser, …)`.
    // TODO-VERIFY: generated instruction name/args for the global-factor
    // insert (`ConfigOps::insert_global_factor_by_key` in the SDK) and
    // its accounts struct; not load-bearing for the assertions.
    set_referred_user_discount_factor(ctx, keeper, store, REFERRED_USER_DISCOUNT_FACTOR).await
}

pub async fn set_referred_user_discount_factor(
    ctx: &mut solana_program_test::ProgramTestContext,
    keeper: &Keypair,
    store: &Pubkey,
    factor: u128,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO-VERIFY: see `initialize_gt` note above.
    let _ = (ctx, keeper, store, factor);
    Err("TODO-VERIFY: wire global factor insert (see comment)".into())
}

pub async fn initialize_user(
    ctx: &mut solana_program_test::ProgramTestContext,
    owner: &Keypair,
    store: &Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO-VERIFY: generated instruction name (expected
    // `instruction::InitializeUser`) and accounts
    // (expected `{ owner, store, user, system_program }`); the user PDA
    // is `user_pda(store, owner)`.
    let accounts = gmsol_store::accounts::InitializeUser {
        owner: owner.pubkey(),
        store: *store,
        user: user_pda(store, &owner.pubkey()),
        system_program: solana_sdk::system_program::ID,
    };
    let args = gmsol_store::instruction::InitializeUser {};
    send(&mut ctx.banks_client, owner, &[], vec![ix!(accounts, args)]).await
}

pub async fn initialize_referral_code(
    ctx: &mut solana_program_test::ProgramTestContext,
    owner: &Keypair,
    store: &Pubkey,
    code: [u8; 8],
) -> Result<(), Box<dyn std::error::Error>> {
    // Accounts from `instructions/user.rs::InitializeReferralCode`
    // (tail fields source-verified; see setup note).
    let accounts = gmsol_store::accounts::InitializeReferralCode {
        owner: owner.pubkey(),
        store: *store,
        referral_code: referral_code_pda(store, &code),
        user: user_pda(store, &owner.pubkey()),
        system_program: solana_sdk::system_program::ID,
    };
    let args = gmsol_store::instruction::InitializeReferralCode { code };
    send(&mut ctx.banks_client, owner, &[], vec![ix!(accounts, args)]).await
}

pub async fn fund_collateral(
    ctx: &mut solana_program_test::ProgramTestContext,
    payer: &Keypair,
    owner: &Pubkey,
    mint: &Pubkey,
    amount: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO-VERIFY: the fixture mints are created inside
    // `setup_oracle_and_tokens` with the test payer as mint authority;
    // this helper creates the owner's ATA (associated token program)
    // and mints `amount` into it with plain SPL token instructions.
    let _ = (ctx, payer, owner, mint, amount);
    Err("TODO-VERIFY: wire ATA creation + mint_to (see comment)".into())
}

pub async fn seed_market_vaults(
    ctx: &mut solana_program_test::ProgramTestContext,
    payer: &Keypair,
    store: &Pubkey,
    long_token: &Pubkey,
    short_token: &Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO-VERIFY: market vault token accounts are the ATAs of the
    // store wallet (`market_vault_pda` derives the vault *addresses*
    // used by the execute instruction; the token accounts themselves
    // are ATAs — confirm against `create_market`'s initialisation).
    // Seed 10,000 long + 1,000,000 short so the increase execution has
    // pool liquidity.
    let _ = (ctx, payer, store, long_token, short_token);
    Err("TODO-VERIFY: wire market vault seeding (see comment)".into())
}

// ===========================================================================
// Lifecycle instructions (field lists read from the program source)
// ===========================================================================

/// `set_referrer` — accounts source-verified in
/// `programs/store/src/instructions/user.rs::SetReferrer`.
/// Guards (for the record): `referrer_user.key() != user.key()`
/// (SelfReferral — a second wallet passes), the referrer's recorded
/// referrer must not be the trader (MutualReferral). Nothing checks
/// *when* the referrer is set relative to order execution — that is
/// the bug.
pub async fn set_referrer(
    ctx: &mut solana_program_test::ProgramTestContext,
    fx: &Fixture,
    trader: &Keypair,
    code: [u8; 8],
    referrer_owner: &Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let accounts = gmsol_store::accounts::SetReferrer {
        owner: trader.pubkey(),
        store: fx.store,
        user: user_pda(&fx.store, &trader.pubkey()),
        referral_code: referral_code_pda(&fx.store, &code),
        referrer_user: user_pda(&fx.store, referrer_owner),
    };
    let args = gmsol_store::instruction::SetReferrer { code };
    send(&mut ctx.banks_client, trader, &[], vec![ix!(accounts, args)]).await
}

/// `create_order_v2` for a market-increase, long, collateral = long token.
/// Accounts from `instructions/exchange/order.rs::CreateOrderV2`.
/// Remaining accounts: none (swap path length 0).
pub async fn create_order(
    ctx: &mut solana_program_test::ProgramTestContext,
    fx: &Fixture,
    nonce: [u8; 32],
) -> Result<Pubkey, Box<dyn std::error::Error>> {
    let owner = fx.trader.pubkey();
    let order = order_pda(&fx.store, &owner, &nonce);
    // TODO-VERIFY: `PositionKind::Long` discriminant byte in the
    // position PDA seeds (from `params.to_position_kind()`; the kind is
    // stored as `u8` in the Position account).
    let position = position_pda(&fx.store, &owner, &fx.market_token_mint, &fx.long_token, POSITION_KIND_LONG);
    let collateral_escrow = associated_token_address(&order, &fx.long_token);
    let accounts = gmsol_store::accounts::CreateOrderV2 {
        owner,
        receiver: owner,
        store: fx.store,
        market: fx.market,
        user: user_pda(&fx.store, &owner),
        order,
        position: Some(position),
        initial_collateral_token: fx.long_token,
        final_output_token: fx.long_token, // increase: must equal collateral token
        long_token: fx.long_token,
        short_token: fx.short_token,
        // Collateral == long token, so the initial-collateral escrow and
        // the long-token escrow are the same ATA of the order account.
        // TODO-VERIFY: passing the identical ATA for both fields is the
        // expected shape for collateral == long (the close handler
        // explicitly dedups aliased escrows).
        initial_collateral_token_escrow: Some(collateral_escrow),
        final_output_token_escrow: None,
        long_token_escrow: Some(collateral_escrow),
        short_token_escrow: Some(associated_token_address(&order, &fx.short_token)),
        initial_collateral_token_source: Some(associated_token_address(&owner, &fx.long_token)),
        system_program: solana_sdk::system_program::ID,
        token_program: anchor_spl::token::ID,
        associated_token_program: anchor_spl::associated_token::ID,
        callback_authority: None,
        callback_program: None,
        callback_shared_data_account: None,
        callback_partitioned_data_account: None,
        // TODO-VERIFY: `#[event_cpi]` generated fields
        // (`event_authority`, `program`) — Anchor 0.31 generated clients
        // append both to event-CPI instructions; values are the
        // program-derived event authority and `gmsol_store::ID`.
        event_authority: event_authority_pda(),
        program: gmsol_store::ID,
    };
    // TODO-VERIFY: import path of `CreateOrderParams`
    // (defined in `programs/store/src/ops/order.rs`; expected
    // re-export `gmsol_store::instructions::CreateOrderParams` or
    // `gmsol_utils::order::CreateOrderParams`) and the
    // `should_unwrap_native_token` field type (bool).
    let params = gmsol_store::instructions::CreateOrderParams {
        kind: gmsol_store::instructions::OrderKind::MarketIncrease,
        decrease_position_swap_type: None,
        execution_lamports: 0, // TODO-VERIFY: minimum execution fee the handler requires, if any
        swap_path_length: 0,
        initial_collateral_delta_amount: COLLATERAL_AMOUNT,
        size_delta_value: SIZE_DELTA_VALUE,
        is_long: true,
        is_collateral_long: true,
        min_output: None,
        trigger_price: None,
        acceptable_price: None,
        should_unwrap_native_token: false,
        valid_from_ts: None,
    };
    let args = gmsol_store::instruction::CreateOrderV2 { nonce, params };
    send(&mut ctx.banks_client, &fx.trader, &[], vec![ix!(accounts, args)]).await?;
    Ok(order)
}

/// `execute_increase_or_swap_order_v2` by the keeper.
/// Accounts from `instructions/exchange/execute_order.rs`.
/// Remaining accounts: the price-feed accounts (no swap path).
pub async fn execute_order(
    ctx: &mut solana_program_test::ProgramTestContext,
    fx: &Fixture,
    order: &Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let owner = fx.trader.pubkey();
    let collateral_escrow = associated_token_address(order, &fx.long_token);
    let accounts = gmsol_store::accounts::ExecuteIncreaseOrSwapOrderV2 {
        authority: fx.keeper.pubkey(),
        store: fx.store,
        token_map: fx.token_map,
        oracle: fx.oracle,
        market: fx.market,
        owner,
        user: user_pda(&fx.store, &owner),
        order: *order,
        position: Some(position_pda(
            &fx.store,
            &owner,
            &fx.market_token_mint,
            &fx.long_token,
            POSITION_KIND_LONG,
        )),
        // TODO-VERIFY: the optional TradeData "event" buffer — anchor_test
        // executions run without a per-keeper trade buffer in the common
        // path; if the handler requires it, create the keeper's
        // TradeData account in the fixture and pass `Some(..)`.
        event: None,
        initial_collateral_token: fx.long_token,
        final_output_token: fx.long_token,
        long_token: fx.long_token,
        short_token: fx.short_token,
        initial_collateral_token_escrow: Some(collateral_escrow),
        final_output_token_escrow: None,
        long_token_escrow: Some(collateral_escrow),
        short_token_escrow: Some(associated_token_address(order, &fx.short_token)),
        initial_collateral_token_vault: Some(market_vault_pda(&fx.store, &fx.long_token)),
        final_output_token_vault: Some(market_vault_pda(&fx.store, &fx.long_token)),
        long_token_vault: Some(market_vault_pda(&fx.store, &fx.long_token)),
        short_token_vault: Some(market_vault_pda(&fx.store, &fx.short_token)),
        token_program: anchor_spl::token::ID,
        system_program: solana_sdk::system_program::ID,
        callback_authority: None,
        callback_program: None,
        callback_shared_data_account: None,
        callback_partitioned_data_account: None,
        // TODO-VERIFY: event-CPI generated fields (see create_order).
        event_authority: event_authority_pda(),
        program: gmsol_store::ID,
    };
    let recent_timestamp = ctx
        .banks_client
        .get_sysvar::<solana_sdk::clock::Clock>()
        .await?
        .unix_timestamp;
    let args = gmsol_store::instruction::ExecuteIncreaseOrSwapOrderV2 { recent_timestamp };
    let mut metas = accounts.to_account_metas(None);
    // Remaining accounts: feed accounts first (swap path is empty).
    for feed in &fx.feed_accounts {
        metas.push(anchor_lang::solana_program::instruction::AccountMeta::new_readonly(
            *feed, false,
        ));
    }
    let instruction = Instruction {
        program_id: gmsol_store::ID,
        accounts: metas,
        data: args.data(),
    };
    send(&mut ctx.banks_client, &fx.keeper, &[], vec![instruction]).await
}

/// `close_order_v2`, called by the trader as executor (= owner path;
/// a keeper closing a terminal order runs the same `process_gt_reward`).
/// Accounts source-verified in `instructions/exchange/order.rs::CloseOrderV2`.
/// `referrer_at_close` is the owner key of whoever
/// `user.referral().referrer()` names *at close time* — the account
/// constraint re-reads it from the User account, so this must match the
/// post-switch state. That is precisely the mechanism under test.
pub async fn close_order(
    ctx: &mut solana_program_test::ProgramTestContext,
    fx: &Fixture,
    order: &Pubkey,
    referrer_at_close: Option<&Pubkey>,
) -> Result<(), Box<dyn std::error::Error>> {
    let owner = fx.trader.pubkey();
    let collateral_escrow = associated_token_address(order, &fx.long_token);
    let accounts = gmsol_store::accounts::CloseOrderV2 {
        executor: owner,
        store: fx.store,
        store_wallet: store_wallet_pda(&fx.store),
        owner,
        receiver: owner,
        rent_receiver: owner,
        user: user_pda(&fx.store, &owner),
        referrer_user: referrer_at_close.map(|r| user_pda(&fx.store, r)),
        order: *order,
        initial_collateral_token: fx.long_token,
        final_output_token: fx.long_token,
        long_token: fx.long_token,
        short_token: fx.short_token,
        initial_collateral_token_escrow: Some(collateral_escrow),
        final_output_token_escrow: None,
        long_token_escrow: Some(collateral_escrow),
        short_token_escrow: Some(associated_token_address(order, &fx.short_token)),
        initial_collateral_token_ata: Some(associated_token_address(&owner, &fx.long_token)),
        final_output_token_ata: Some(associated_token_address(&owner, &fx.long_token)),
        long_token_ata: Some(associated_token_address(&owner, &fx.long_token)),
        short_token_ata: Some(associated_token_address(&owner, &fx.short_token)),
        system_program: solana_sdk::system_program::ID,
        token_program: anchor_spl::token::ID,
        associated_token_program: anchor_spl::associated_token::ID,
        callback_authority: None,
        callback_program: None,
        callback_shared_data_account: None,
        callback_partitioned_data_account: None,
        // TODO-VERIFY: event-CPI generated fields (see create_order).
        event_authority: event_authority_pda(),
        program: gmsol_store::ID,
    };
    // TODO-VERIFY: `close_order_v2` takes no user args (any hint/param
    // struct in the generated instruction would appear here).
    let args = gmsol_store::instruction::CloseOrderV2 {};
    send(&mut ctx.banks_client, &fx.trader, &[], vec![ix!(accounts, args)]).await
}

/// TODO-VERIFY: `PositionKind::Long` as stored/seeds byte.
pub const POSITION_KIND_LONG: u8 = 0;

/// The event authority PDA for `#[event_cpi]` instructions.
/// TODO-VERIFY: derivation (Anchor convention:
/// `find_program_address(&[b"__event_authority"], &program_id)`).
pub fn event_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &gmsol_store::ID).0
}

/// ATA derivation without an extra dependency:
/// `get_associated_token_address` from the associated-token program.
/// TODO-VERIFY: use `anchor_spl::associated_token::get_associated_token_address`
/// (re-export) — the manual derivation below matches its definition.
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), anchor_spl::token::ID.as_ref(), mint.as_ref()],
        &anchor_spl::associated_token::ID,
    )
    .0
}

// ===========================================================================
// Case driver
// ===========================================================================

pub struct CaseOutcome {
    /// `order.gt_reward` recorded at execution (GT base units).
    pub gt_reward: u64,
    /// GT ledger delta of X across the close.
    pub x_delta_close: u64,
    /// GT ledger delta of B across the close.
    pub b_delta_close: u64,
    /// GT ledger delta of the trader across the close (expected 0 in
    /// every case: the trader's own GT is minted at execution).
    pub trader_delta_close: u64,
    /// `GtState.supply` delta across the close (= total referral GT
    /// minted by the close; the trader's execution mint happens earlier).
    pub supply_delta_close: u64,
}

/// Full lifecycle: [set initial referrer] → create → execute →
/// [switch referrer] → close, with formula cross-checks at execution.
pub async fn run_case(
    ctx: &mut solana_program_test::ProgramTestContext,
    fx: &Fixture,
    nonce: [u8; 32],
    initial_referrer: Option<([u8; 8], &Pubkey)>,
    switch_to: Option<([u8; 8], &Pubkey)>,
) -> Result<CaseOutcome, Box<dyn std::error::Error>> {
    let trader_owner = fx.trader.pubkey();
    let x_owner = fx.referrer_x.pubkey();
    let b_owner = fx.referrer_b.pubkey();

    if let Some((code, referrer_owner)) = initial_referrer {
        set_referrer(ctx, fx, &fx.trader, code, referrer_owner).await?;
    }

    // --- Execute -------------------------------------------------------------
    let trader_before = gt_snapshot(&mut ctx.banks_client, &fx.store, &trader_owner).await?;
    let (_, _, minting_cost_before) = gt_globals(&mut ctx.banks_client, &fx.store).await?;

    let order = create_order(ctx, fx, nonce).await?;
    post_mock_prices(ctx, fx).await?;
    execute_order(ctx, fx, &order).await?;

    let trader_after_exec = gt_snapshot(&mut ctx.banks_client, &fx.store, &trader_owner).await?;
    let (_, _, minting_cost_after) = gt_globals(&mut ctx.banks_client, &fx.store).await?;
    assert_eq!(
        minting_cost_before, minting_cost_after,
        "fixture must not cross a GT grow step mid-case (formula mirror assumes a fixed cost)"
    );

    // TODO-VERIFY: `Order::gt_reward` external readability (field or
    // getter) in the generated/state types; the counter-based
    // expectation below is the primary cross-check either way.
    let order_state = load_zero_copy::<gmsol_store::states::Order>(&mut ctx.banks_client, &order).await?;
    let gt_reward = order_state.gt_reward;

    let expected = expected_gt_reward(
        trader_before.paid_fee_value,
        trader_before.minted_fee_value,
        trader_after_exec.paid_fee_value,
        minting_cost_before,
    );
    assert!(gt_reward > 0, "order must have paid fees and earned GT");
    assert_eq!(
        gt_reward, expected,
        "order.gt_reward must equal fee_value / minting_cost (unchecked_process_gt)"
    );
    assert_eq!(
        trader_after_exec.amount - trader_before.amount,
        gt_reward,
        "trader's own GT is minted at execution"
    );

    // --- Switch (the attack step; a plain user instruction) ------------------
    if let Some((code, referrer_owner)) = switch_to {
        set_referrer(ctx, fx, &fx.trader, code, referrer_owner).await?;
    }

    // --- Close ---------------------------------------------------------------
    let x_before = gt_snapshot(&mut ctx.banks_client, &fx.store, &x_owner).await?;
    let b_before = gt_snapshot(&mut ctx.banks_client, &fx.store, &b_owner).await?;
    let (supply_before, _, _) = gt_globals(&mut ctx.banks_client, &fx.store).await?;

    // Whoever is referrer *now* determines the close's referrer_user.
    let referrer_at_close = match (switch_to, initial_referrer) {
        (Some((_, owner)), _) => Some(*owner),
        (None, Some((_, owner))) => Some(*owner),
        (None, None) => None,
    };
    close_order(ctx, fx, &order, referrer_at_close.as_ref()).await?;

    let x_after = gt_snapshot(&mut ctx.banks_client, &fx.store, &x_owner).await?;
    let b_after = gt_snapshot(&mut ctx.banks_client, &fx.store, &b_owner).await?;
    let trader_after_close = gt_snapshot(&mut ctx.banks_client, &fx.store, &trader_owner).await?;
    let (supply_after, _, _) = gt_globals(&mut ctx.banks_client, &fx.store).await?;

    // The order account is closed by the close instruction.
    assert!(
        ctx.banks_client.get_account(order).await?.is_none(),
        "order account must be closed"
    );
    // Rank precondition actually used by every assertion below.
    assert_eq!(x_before.rank, 0, "X must be rank 0 at close");
    assert_eq!(b_before.rank, 0, "B must be rank 0 at close");

    Ok(CaseOutcome {
        gt_reward,
        x_delta_close: x_after.amount - x_before.amount,
        b_delta_close: b_after.amount - b_before.amount,
        trader_delta_close: trader_after_close.amount - trader_after_exec.amount,
        supply_delta_close: supply_after - supply_before,
    })
}

pub fn program_test() -> ProgramTest {
    // TODO-VERIFY: register the mock Chainlink verifier `.so` here if
    // `setup_oracle_and_tokens` requires it
    // (`ProgramTest::add_program("gmsol_mock_chainlink_verifier", …)`),
    // mirroring the genesis programs in `Anchor.toml`.
    ProgramTest::new("gmsol_store", gmsol_store::ID, None)
}

// ===========================================================================
// Tests
// ===========================================================================

/// Case 1 — DIVERSION (the High claim). U executes referred by X
/// (the protocol evaluates X for the execution-time discount), then
/// switches referrer to own alt B, then closes.
/// Expected: B is minted `gt_reward × 0.50` (rank 0, deployed table);
/// X — the referrer of record when the reward was *earned* — gets 0.
#[tokio::test]
async fn case_1_referrer_switch_before_close_diverts_bonus_to_alt() {
    let mut ctx = program_test().start_with_context().await;
    let fx = setup_fixture(&mut ctx).await.expect("fixture");
    let out = run_case(
        &mut ctx,
        &fx,
        [0xC1; 32],
        Some((CODE_X, &fx.referrer_x.pubkey())),
        Some((CODE_B, &fx.referrer_b.pubkey())),
    )
    .await
    .expect("lifecycle");

    let expected_bonus = expected_referral_bonus(out.gt_reward, FACTOR_RANK_0);
    assert!(expected_bonus > 0);
    assert_eq!(out.b_delta_close, expected_bonus, "alt B must receive the diverted bonus");
    assert_eq!(out.x_delta_close, 0, "genuine referrer X must receive nothing");
    assert_eq!(out.supply_delta_close, expected_bonus, "close mints exactly the bonus");
    assert_eq!(out.trader_delta_close, 0);
}

/// Case 2 — CONTROL (no switch). Same flow, referrer stays X.
/// Expected: X receives `gt_reward × 0.50`; B receives 0. Together with
/// Case 1 this isolates the post-execution `set_referrer` as the sole
/// cause of the diversion. (Amounts in Cases 1 and 2 are equal by
/// construction — both referrers are rank 0 — so the proof is in the
/// payee deltas, not in a total-minted difference.)
#[tokio::test]
async fn case_2_no_switch_pays_genuine_referrer() {
    let mut ctx = program_test().start_with_context().await;
    let fx = setup_fixture(&mut ctx).await.expect("fixture");
    let out = run_case(
        &mut ctx,
        &fx,
        [0xC2; 32],
        Some((CODE_X, &fx.referrer_x.pubkey())),
        None,
    )
    .await
    .expect("lifecycle");

    let expected_bonus = expected_referral_bonus(out.gt_reward, FACTOR_RANK_0);
    assert!(expected_bonus > 0);
    assert_eq!(out.x_delta_close, expected_bonus);
    assert_eq!(out.b_delta_close, 0);
    assert_eq!(out.supply_delta_close, expected_bonus);
    assert_eq!(out.trader_delta_close, 0);
}

/// Case 3 — CREATION FROM NOTHING. U executes with **no** referrer
/// (at execution, no referral bonus can ever arise), then sets
/// referrer = B, then closes.
/// Expected: B is minted `gt_reward × 0.50` — a bonus created purely
/// by the post-execution `set_referrer`. Case 4 is the counterfactual.
#[tokio::test]
async fn case_3_unreferred_execution_then_set_referrer_creates_bonus() {
    let mut ctx = program_test().start_with_context().await;
    let fx = setup_fixture(&mut ctx).await.expect("fixture");
    let out = run_case(
        &mut ctx,
        &fx,
        [0xC3; 32],
        None,
        Some((CODE_B, &fx.referrer_b.pubkey())),
    )
    .await
    .expect("lifecycle");

    let expected_bonus = expected_referral_bonus(out.gt_reward, FACTOR_RANK_0);
    assert!(expected_bonus > 0);
    assert_eq!(out.b_delta_close, expected_bonus, "bonus minted for a referral that did not exist at execution");
    assert_eq!(out.x_delta_close, 0);
    assert_eq!(out.supply_delta_close, expected_bonus);
    assert_eq!(out.trader_delta_close, 0);
}

/// Case 4 — CONTROL (never referred). U executes and closes with no
/// referrer at any point. `mint_gt_reward_for_referrer` early-returns
/// on `referrer == None`.
/// Expected: the close mints zero GT to anyone; total GT minted over
/// the lifecycle is exactly the trader's own execution mint. This is
/// the baseline that makes Case 3's bonus "created from nothing".
#[tokio::test]
async fn case_4_no_referrer_no_bonus() {
    let mut ctx = program_test().start_with_context().await;
    let fx = setup_fixture(&mut ctx).await.expect("fixture");
    let out = run_case(&mut ctx, &fx, [0xC4; 32], None, None)
        .await
        .expect("lifecycle");

    assert!(out.gt_reward > 0, "the order did earn GT for the trader");
    assert_eq!(out.b_delta_close, 0);
    assert_eq!(out.x_delta_close, 0);
    assert_eq!(out.supply_delta_close, 0, "no referral GT may be minted at close");
    assert_eq!(out.trader_delta_close, 0);
}
