//! PoC: GMTrade GT-exchange freeze for separate-rent-payer exchanges.
//!
//! Finding (see `~/workspace/bugbounty/gmtrade-q1-verification.md`):
//! a GT exchange created via Store `request_gt_exchange_v2` with
//! `payer != owner` records `rent_receiver = payer` (#493 `f102501f`).
//! Treasury `complete_gt_exchange` — the only payout path — CPIs first to
//! Store V1 `close_gt_exchange`, whose Accounts carry
//! `constraint = exchange.rent_receiver() == owner`
//! (#479 `884d121d`), so completion always reverts with
//! `CoreError::RentReceiverMismatched`. Store `close_gt_exchange_v2` is
//! GT-controller-gated and pays nothing, so the claim is permanently
//! frozen (Immunefi GMTrade, High: "Permanent freezing of unclaimed
//! yield").
//!
//! Placement: `programs/treasury/tests/gt_exchange_freeze.rs` in
//! `gmsol-labs/gmx-solana` (analysed at `main`
//! `a10a6599a42442aac2b4873fb058616a40e07f27`). Dev-dependencies to add
//! are listed in the accompanying README / `Cargo.dev-deps.snippet.toml`.
//!
//! STATUS: implemented, NOT yet executed (the authoring VM has no Rust
//! toolchain). Spots that could not be verified from source alone carry
//! `TODO-VERIFY` comments; everything else mirrors account layouts and
//! builder patterns read from the repository itself:
//!   - `programs/treasury/tests/complete_gt_exchange.rs` (generated
//!     client pattern + `CompleteGtExchange` / V1 `CloseGtExchange`
//!     account lists),
//!   - `tests/anchor_test/gt_exchange.rs` (V2 close accounts, error-code
//!     assertion style, GT amounts),
//!   - `tests/anchor_test/setup.rs` (`initialize_gt` arguments, role
//!     grants, store initialisation),
//!   - `crates/sdk/src/client/ops/{gt,treasury,store,user,role}.rs` and
//!     `crates/sdk/src/builders/gt.rs` (every instruction's accounts),
//!   - `crates/sdk/src/pda.rs` (all PDA derivations, reimplemented here
//!     from the same seed constants so the test does not need the SDK's
//!     RPC client),
//!   - `programs/treasury/src/instructions/{gt_bank,treasury}.rs`
//!     (handler semantics: close-first completion, deposit-time balance
//!     recording, buyback-time bank confirmation).
//!
//! Design corrections vs. the original design doc, found while reading
//! the handlers (see README for details):
//!   1. Funding the GT bank is done with `deposit_to_treasury_vault`
//!      (records the token balance in the bank), NOT by minting into the
//!      bank vault + `sync_gt_bank_v2` (sync only sweeps unrecorded
//!      excess into the treasury vault).
//!   2. `gt_set_exchange_time_window` is gated behind the store's
//!      `test-only` feature in the shipped program, so this test uses
//!      the default exchange window and crosses it by warping the
//!      `Clock` sysvar.
//!   3. The bank's `remaining_confirmed_gt_amount` is set only by
//!      `confirm_gt_buyback` (oracle-priced). The Tier-1 tests therefore
//!      prove the freeze at the close seam (which runs *before* any bank
//!      accounting in `complete_gt_exchange`) and isolate the
//!      rent-receiver check as the only difference in the control case;
//!      the full positive-control payout is the `#[ignore]`d Tier-2
//!      test at the bottom.

use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::{InstructionData, ToAccountMetas};
use solana_program_test::{ProgramTest, ProgramTestContext};
use solana_sdk::{
    clock::Clock,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction, InstructionError},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::{Transaction, TransactionError},
};

use gmsol_store::constants::{DEFAULT_GT_VAULT_TIME_WINDOW, MARKET_USD_UNIT};
use gmsol_utils::gt::get_time_window_index;

// ---------------------------------------------------------------------------
// Constants (mirroring tests/anchor_test where applicable)
// ---------------------------------------------------------------------------

/// GT minted to the owner (raw units; the anchor_test GT test mints 2).
const GT_MINTED: u64 = 2;
/// GT exchanged per scenario (leaves a remainder of 1 visible).
const EXCHANGE_AMOUNT: u64 = 1;
/// Payout-token units funded into the GT bank via the receiver vault.
const BANK_FUNDS: u64 = 1_000_000;
/// Payout token decimals (fixture-local mint).
const PAYOUT_DECIMALS: u8 = 6;
/// GT decimals, mirroring `Deployment::initialize_gt(7)` in setup.rs.
const GT_DECIMALS: u8 = 7;
/// Store key: the default (empty) store, as in `find_default_store_address`.
const STORE_KEY: &str = "";

// Role names as stored in the Store (treasury names are the string
// constants of `programs/treasury/src/roles.rs`; the two Store names are
// the `RoleKey` variants used throughout `tests/anchor_test/setup.rs`).
// TODO-VERIFY: `gmsol_store` does not export its role-name constants
// publicly; if a grant fails with a role error, compare against
// `gmsol_utils::role::RoleKey::{GT_CONTROLLER, MARKET_KEEPER}` strings.
const ROLE_GT_CONTROLLER: &str = "GT_CONTROLLER";
const ROLE_MARKET_KEEPER: &str = "MARKET_KEEPER";
const ROLE_TREASURY_ADMIN: &str = "TREASURY_ADMIN";
const ROLE_TREASURY_KEEPER: &str = "TREASURY_KEEPER";
const ROLE_TREASURY_WITHDRAWER: &str = "TREASURY_WITHDRAWER";

// ---------------------------------------------------------------------------
// PDA derivations (seed constants as in crates/sdk/src/pda.rs)
// ---------------------------------------------------------------------------

fn store_address() -> Pubkey {
    let seed = solana_sdk::hash::hash(STORE_KEY.as_bytes()).to_bytes();
    Pubkey::find_program_address(&[b"data_store", &seed], &gmsol_store::ID).0
}

fn user_address(store: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"user", store.as_ref(), owner.as_ref()],
        &gmsol_store::ID,
    )
    .0
}

fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &gmsol_store::ID).0
}

fn gt_exchange_vault_address(store: &Pubkey, index: i64, window: u32) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"gt_exchange_vault",
            store.as_ref(),
            &index.to_le_bytes(),
            &window.to_le_bytes(),
        ],
        &gmsol_store::ID,
    )
    .0
}

fn gt_exchange_address(vault: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"gt_exchange", vault.as_ref(), owner.as_ref()],
        &gmsol_store::ID,
    )
    .0
}

fn treasury_config_address(store: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"config", store.as_ref()], &gmsol_treasury::ID).0
}

fn treasury_receiver_address(config: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"receiver", config.as_ref()], &gmsol_treasury::ID).0
}

fn treasury_vault_config_address(config: &Pubkey, index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"treasury_vault_config",
            config.as_ref(),
            &index.to_le_bytes(),
        ],
        &gmsol_treasury::ID,
    )
    .0
}

fn gt_bank_address(treasury_vault_config: &Pubkey, gt_exchange_vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"gt_bank",
            treasury_vault_config.as_ref(),
            gt_exchange_vault.as_ref(),
        ],
        &gmsol_treasury::ID,
    )
    .0
}

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address(owner, mint)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn ix(
    program_id: Pubkey,
    accounts: impl ToAccountMetas,
    data: impl InstructionData,
) -> Instruction {
    Instruction {
        program_id,
        accounts: accounts.to_account_metas(None),
        data: data.data(),
    }
}

fn ix_with_remaining(
    program_id: Pubkey,
    accounts: impl ToAccountMetas,
    data: impl InstructionData,
    remaining: Vec<AccountMeta>,
) -> Instruction {
    let mut metas = accounts.to_account_metas(None);
    metas.extend(remaining);
    Instruction {
        program_id,
        accounts: metas,
        data: data.data(),
    }
}

async fn blockhash(ctx: &mut ProgramTestContext) -> Hash {
    ctx.banks_client
        .get_latest_blockhash()
        .await
        .expect("latest blockhash")
}

/// Send a transaction signed by the context payer plus `signers`.
async fn send(
    ctx: &mut ProgramTestContext,
    ixs: &[Instruction],
    signers: &[&Keypair],
) -> Result<(), solana_program_test::BanksClientError> {
    let payer = ctx.payer.insecure_clone();
    let mut refs: Vec<&Keypair> = vec![&payer];
    for s in signers {
        if s.pubkey() != payer.pubkey() {
            refs.push(s);
        }
    }
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&payer.pubkey()),
        &refs,
        blockhash(ctx).await,
    );
    ctx.banks_client.process_transaction(tx).await
}

async fn send_ok(
    ctx: &mut ProgramTestContext,
    ixs: &[Instruction],
    signers: &[&Keypair],
    what: &str,
) {
    send(ctx, ixs, signers)
        .await
        .unwrap_or_else(|e| panic!("{what} should succeed: {e:?}"));
}

/// Extract the Anchor custom error code from a failed transaction.
fn custom_code(err: &solana_program_test::BanksClientError) -> Option<u32> {
    match err {
        solana_program_test::BanksClientError::TransactionError(
            TransactionError::InstructionError(_, InstructionError::Custom(code)),
        ) => Some(*code),
        _ => None,
    }
}

fn rent_receiver_mismatched_code() -> u32 {
    // Same conversion the repo's anchor_test asserts with:
    // `CoreError::RentReceiverMismatched.into()`.
    gmsol_store::CoreError::RentReceiverMismatched.into()
}

async fn lamports(ctx: &mut ProgramTestContext, address: &Pubkey) -> u64 {
    ctx.banks_client
        .get_account(*address)
        .await
        .expect("get_account")
        .map(|a| a.lamports)
        .unwrap_or(0)
}

async fn account_data(ctx: &mut ProgramTestContext, address: &Pubkey) -> Option<Vec<u8>> {
    ctx.banks_client
        .get_account(*address)
        .await
        .expect("get_account")
        .map(|a| a.data)
}

async fn token_amount(ctx: &mut ProgramTestContext, address: &Pubkey) -> u64 {
    let data = account_data(ctx, address)
        .await
        .unwrap_or_else(|| panic!("token account {address} missing"));
    anchor_spl::token::spl_token::state::Account::unpack(&data)
        .expect("unpack token account")
        .amount
}

/// Decode a `#[account(zero_copy)]` struct from account data.
/// TODO-VERIFY: this assumes the Anchor zero-copy layout
/// (8-byte discriminator followed by the `repr(C)` struct) and that the
/// generated types implement `bytemuck::Pod`, which is how
/// `gmsol_sdk::utils::zero_copy::ZeroCopy<T>` reads the same accounts.
/// If it does not hold, swap this helper for the SDK's `ZeroCopy<T>`.
fn load_zero_copy<T: bytemuck::Pod>(data: &[u8]) -> T {
    // Anchor account discriminator length (8 bytes). NOTE:
    // `anchor_lang::Discriminator` is a trait, so its associated
    // constant cannot be referenced without a concrete type
    // (run #5's E0782); the literal keeps the offset logic above.
    let start = 8;
    let end = start + std::mem::size_of::<T>();
    *bytemuck::from_bytes::<T>(&data[start..end])
}

async fn load_gt_exchange(
    ctx: &mut ProgramTestContext,
    exchange: &Pubkey,
) -> gmsol_store::states::gt::GtExchange {
    let data = account_data(ctx, exchange)
        .await
        .expect("exchange account exists");
    load_zero_copy::<gmsol_store::states::gt::GtExchange>(&data)
}

async fn load_gt_bank(
    ctx: &mut ProgramTestContext,
    gt_bank: &Pubkey,
) -> gmsol_treasury::states::GtBank {
    let data = account_data(ctx, gt_bank)
        .await
        .expect("gt bank account exists");
    load_zero_copy::<gmsol_treasury::states::GtBank>(&data)
}

async fn current_clock(ctx: &mut ProgramTestContext) -> Clock {
    let account = ctx
        .banks_client
        .get_account(solana_sdk::sysvar::clock::id())
        .await
        .expect("get clock")
        .expect("clock sysvar exists");
    bincode::deserialize::<Clock>(&account.data).expect("deserialize clock")
}

/// Warp the clock so the current time-window index is at least
/// `vault_index + 1` (the vault becomes confirmable).
async fn warp_past_window(ctx: &mut ProgramTestContext, window: u32) {
    let clock = current_clock(ctx).await;
    let index = get_time_window_index(clock.unix_timestamp, i64::from(window));
    let target = (index + 2) * i64::from(window);
    ctx.set_sysvar(&Clock {
        unix_timestamp: target,
        ..clock
    });
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

#[allow(dead_code)] // some fields are kept for assertion readability / Tier 2
struct Fixture {
    ctx: ProgramTestContext,
    /// Store authority; also the ProgramTest fee payer.
    admin: Keypair,
    keeper: Keypair,
    owner: Keypair,
    sponsor: Keypair,
    delegate: Keypair,
    store: Pubkey,
    config: Pubkey,
    treasury_vault_config: Pubkey,
    gt_bank: Pubkey,
    vault: Pubkey,
    vault_index: i64,
    window: u32,
    exchange: Pubkey,
    payout_mint: Pubkey,
    receiver_vault: Pubkey,
    treasury_vault: Pubkey,
    gt_bank_vault: Pubkey,
    owner_target: Pubkey,
}

/// Build a fresh ProgramTest instance with Store + GT + Treasury fully
/// initialised and the GT bank funded (recorded balance == BANK_FUNDS),
/// stopping right before the GT exchange request, which differs per
/// scenario.
async fn setup() -> Fixture {
    let mut program_test = ProgramTest::new("gmsol_store", gmsol_store::ID, None);
    program_test.add_program("gmsol_treasury", gmsol_treasury::ID, None);
    // NOTE: the SPL Token and Associated Token programs are included by
    // solana-program-test by default. The .so files are located via
    // BPF_OUT_PATH or the workspace `target/deploy` (see README).
    let mut ctx = program_test.start_with_context().await;

    let admin = ctx.payer.insecure_clone();
    let keeper = Keypair::new();
    let owner = Keypair::new();
    let sponsor = Keypair::new();
    let delegate = Keypair::new();
    let mint_authority = Keypair::new();

    // Fund the actors from the test payer.
    let fund: Vec<Instruction> = [&keeper, &owner, &sponsor, &delegate, &mint_authority]
        .iter()
        .map(|k| {
            system_instruction::transfer(
                &admin.pubkey(),
                &k.pubkey(),
                10_000_000_000, // 10 SOL
            )
        })
        .collect();
    send_ok(&mut ctx, &fund, &[], "fund actors").await;

    let store = store_address();
    let config = treasury_config_address(&store);
    let receiver = treasury_receiver_address(&config);
    let treasury_vault_config = treasury_vault_config_address(&config, 0);
    let window = DEFAULT_GT_VAULT_TIME_WINDOW;

    // -- Store initialisation (mirrors tests/anchor_test/setup.rs:
    //    authority/receiver/holding default to the payer). -------------
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_store::ID,
            gmsol_store::accounts::Initialize {
                payer: admin.pubkey(),
                authority: None,
                receiver: None,
                holding: None,
                store,
                system_program: system_program::ID,
            },
            gmsol_store::instruction::Initialize {
                key: STORE_KEY.to_string(),
            },
        )],
        &[],
        "initialize store",
    )
    .await;

    // -- Enable + grant roles (admin). ---------------------------------
    let roles = [
        ROLE_GT_CONTROLLER,
        ROLE_MARKET_KEEPER,
        ROLE_TREASURY_ADMIN,
        ROLE_TREASURY_KEEPER,
        ROLE_TREASURY_WITHDRAWER,
    ];
    let enable: Vec<Instruction> = roles
        .iter()
        .map(|role| {
            ix(
                gmsol_store::ID,
                gmsol_store::accounts::EnableRole {
                    authority: admin.pubkey(),
                    store,
                },
                gmsol_store::instruction::EnableRole {
                    role: (*role).to_string(),
                },
            )
        })
        .collect();
    send_ok(&mut ctx, &enable, &[], "enable roles").await;

    let grant_keeper: Vec<Instruction> = roles
        .iter()
        .map(|role| {
            ix(
                gmsol_store::ID,
                gmsol_store::accounts::GrantRole {
                    authority: admin.pubkey(),
                    store,
                },
                gmsol_store::instruction::GrantRole {
                    user: keeper.pubkey(),
                    role: (*role).to_string(),
                },
            )
        })
        .collect();
    // Grants are signed by the admin (context payer) alone.
    send_ok(&mut ctx, &grant_keeper, &[], "grant keeper roles").await;

    // -- GT initialisation (keeper, MARKET_KEEPER), arguments copied
    //    verbatim from Deployment::initialize_gt(7) in setup.rs. -------
    let gt_unit = 10u128.pow(u32::from(GT_DECIMALS));
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_store::ID,
            gmsol_store::accounts::InitializeGt {
                authority: keeper.pubkey(),
                store,
                system_program: system_program::ID,
            },
            gmsol_store::instruction::InitializeGt {
                decimals: GT_DECIMALS,
                initial_minting_cost: MARKET_USD_UNIT / 100 / gt_unit,
                grow_factor: 101 * MARKET_USD_UNIT / 100,
                grow_step: (10 * gt_unit) as u64,
                ranks: [10, 30, 300, 1_000, 3_000, 10_000, 30_000, 100_000, 300_000]
                    .iter()
                    .map(|n| (n * gt_unit) as u64)
                    .collect(),
            },
        )],
        &[&keeper],
        "initialize gt",
    )
    .await;

    // -- Treasury config (permissionless init), then grant the Config
    //    PDA GT_CONTROLLER: Treasury's completion CPI calls the
    //    GT-controller-gated V1 close with the Config PDA as authority.
    //    Without this grant even the control case fails. ---------------
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_treasury::ID,
            gmsol_treasury::accounts::InitializeConfig {
                payer: admin.pubkey(),
                store,
                config,
                receiver,
                store_program: gmsol_store::ID,
                system_program: system_program::ID,
            },
            gmsol_treasury::instruction::InitializeConfig {},
        )],
        &[],
        "initialize treasury config",
    )
    .await;

    send_ok(
        &mut ctx,
        &[ix(
            gmsol_store::ID,
            gmsol_store::accounts::GrantRole {
                authority: admin.pubkey(),
                store,
            },
            gmsol_store::instruction::GrantRole {
                user: config,
                role: ROLE_GT_CONTROLLER.to_string(),
            },
        )],
        &[],
        "grant GT_CONTROLLER to treasury config PDA",
    )
    .await;

    // -- Treasury vault config (keeper, TREASURY_ADMIN) + set it as the
    //    config's authorised vault config (required by the
    //    `config.treasury_vault_config() == Some(..)` constraints). ----
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_treasury::ID,
            gmsol_treasury::accounts::InitializeTreasuryVaultConfig {
                authority: keeper.pubkey(),
                store,
                config,
                treasury_vault_config,
                store_program: gmsol_store::ID,
                system_program: system_program::ID,
            },
            gmsol_treasury::instruction::InitializeTreasuryVaultConfig { index: 0 },
        )],
        &[&keeper],
        "initialize treasury vault config",
    )
    .await;

    send_ok(
        &mut ctx,
        &[ix(
            gmsol_treasury::ID,
            gmsol_treasury::accounts::SetTreasuryVaultConfig {
                authority: keeper.pubkey(),
                store,
                config,
                treasury_vault_config,
                store_program: gmsol_store::ID,
            },
            gmsol_treasury::instruction::SetTreasuryVaultConfig {},
        )],
        &[&keeper],
        "set treasury vault config",
    )
    .await;

    // GT factor 100%: deposit_to_treasury_vault routes the whole
    // receiver-vault balance into the GT bank (recorded). Buyback factor
    // 100%: only used by the Tier-2 buyback confirmation.
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_treasury::ID,
            gmsol_treasury::accounts::UpdateConfig {
                authority: keeper.pubkey(),
                store,
                config,
                store_program: gmsol_store::ID,
            },
            gmsol_treasury::instruction::SetGtFactor {
                factor: MARKET_USD_UNIT,
            },
        )],
        &[&keeper],
        "set gt factor",
    )
    .await;
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_treasury::ID,
            gmsol_treasury::accounts::UpdateConfig {
                authority: keeper.pubkey(),
                store,
                config,
                store_program: gmsol_store::ID,
            },
            gmsol_treasury::instruction::SetBuybackFactor {
                factor: MARKET_USD_UNIT,
            },
        )],
        &[&keeper],
        "set buyback factor",
    )
    .await;

    // -- Payout token fixture -------------------------------------------
    let payout_mint = Keypair::new();
    // `BanksClient::get_rent` returns `io::Result<Rent>` in
    // solana-program-test 2.1 (confirmed by run #5's E0599).
    let rent = ctx
        .banks_client
        .get_rent()
        .await
        .expect("get_rent")
        .minimum_balance(anchor_spl::token::spl_token::state::Mint::LEN);
    send_ok(
        &mut ctx,
        &[
            system_instruction::create_account(
                &admin.pubkey(),
                &payout_mint.pubkey(),
                rent,
                anchor_spl::token::spl_token::state::Mint::LEN as u64,
                &anchor_spl::token::ID,
            ),
            anchor_spl::token::spl_token::instruction::initialize_mint(
                &anchor_spl::token::ID,
                &payout_mint.pubkey(),
                &mint_authority.pubkey(),
                None,
                PAYOUT_DECIMALS,
            )
            .expect("initialize_mint ix"),
        ],
        &[&payout_mint],
        "create payout mint",
    )
    .await;

    let receiver_vault = ata(&receiver, &payout_mint.pubkey());
    let treasury_vault = ata(&treasury_vault_config, &payout_mint.pubkey());
    // The GT bank address is known before the bank is prepared (PDA).
    let clock = current_clock(&mut ctx).await;
    let vault_index = get_time_window_index(clock.unix_timestamp, i64::from(window));
    let vault = gt_exchange_vault_address(&store, vault_index, window);
    let gt_bank = gt_bank_address(&treasury_vault_config, &vault);
    let gt_bank_vault = ata(&gt_bank, &payout_mint.pubkey());
    let owner_target = ata(&owner.pubkey(), &payout_mint.pubkey());

    // Create the four ATAs (owner differs per ATA).
    // TODO-VERIFY: the `spl_associated_token_account` re-export path
    // under `anchor_spl::associated_token` (anchor-spl 0.31). If it does
    // not resolve, add `spl-associated-token-account` as a dev-dependency
    // and import from there instead; the instruction itself is unchanged.
    let create_atas: Vec<Instruction> = [
        (&receiver, receiver_vault),
        (&treasury_vault_config, treasury_vault),
        (&gt_bank, gt_bank_vault),
        (&owner.pubkey(), owner_target),
    ]
    .iter()
    .map(|(authority, _)| {
        anchor_spl::associated_token::spl_associated_token_account::instruction::create_associated_token_account(
            &admin.pubkey(),
            authority,
            &payout_mint.pubkey(),
            &anchor_spl::token::ID,
        )
    })
    .collect();
    send_ok(&mut ctx, &create_atas, &[], "create ATAs").await;

    send_ok(
        &mut ctx,
        &[anchor_spl::token::spl_token::instruction::mint_to(
            &anchor_spl::token::ID,
            &payout_mint.pubkey(),
            &receiver_vault,
            &mint_authority.pubkey(),
            &[],
            BANK_FUNDS,
        )
        .expect("mint_to ix")],
        &[&mint_authority],
        "fund receiver vault",
    )
    .await;

    // -- Insert payout token into the vault config + allow deposits. ---
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_treasury::ID,
            gmsol_treasury::accounts::InsertTokenToTreasuryVault {
                authority: keeper.pubkey(),
                store,
                config,
                treasury_vault_config,
                token: payout_mint.pubkey(),
                store_program: gmsol_store::ID,
            },
            gmsol_treasury::instruction::InsertTokenToTreasuryVault {},
        )],
        &[&keeper],
        "insert token to treasury",
    )
    .await;

    // Inserted tokens start with all flags cleared
    // (TokenConfig::default() is zeroed), so deposits/withdrawals must
    // be enabled explicitly.
    // TODO-VERIFY: the exact flag strings parsed by
    // `toggle_token_flag` (the `TokenFlag` variant names in
    // gmsol-utils). If parsing fails, check the strum serialisation of
    // `TokenFlag::{AllowDeposit, AllowWithdrawal}`.
    for flag in ["AllowDeposit", "AllowWithdrawal"] {
        send_ok(
            &mut ctx,
            &[ix(
                gmsol_treasury::ID,
                gmsol_treasury::accounts::ToggleTokenFlag {
                    authority: keeper.pubkey(),
                    store,
                    config,
                    treasury_vault_config,
                    token: payout_mint.pubkey(),
                    store_program: gmsol_store::ID,
                },
                gmsol_treasury::instruction::ToggleTokenFlag {
                    flag: flag.to_string(),
                    value: true,
                },
            )],
            &[&keeper],
            "toggle token flag",
        )
        .await;
    }

    // -- Owner user account + GT balance --------------------------------
    let owner_user = user_address(&store, &owner.pubkey());
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_store::ID,
            gmsol_store::accounts::PrepareUser {
                owner: owner.pubkey(),
                store,
                user: owner_user,
                system_program: system_program::ID,
            },
            gmsol_store::instruction::PrepareUser {},
        )],
        &[&owner],
        "prepare user",
    )
    .await;

    send_ok(
        &mut ctx,
        &[ix(
            gmsol_store::ID,
            gmsol_store::accounts::MintGtReward {
                authority: keeper.pubkey(),
                store,
                user: owner_user,
                event_authority: event_authority(),
                program: gmsol_store::ID,
            },
            gmsol_store::instruction::MintGtReward { amount: GT_MINTED },
        )],
        &[&keeper],
        "mint gt reward",
    )
    .await;

    // -- Prepare the GT exchange vault + GT bank, then fund the bank
    //    through the production path (deposit_to_treasury_vault). -----
    send_ok(
        &mut ctx,
        &[ix(
            gmsol_store::ID,
            gmsol_store::accounts::PrepareGtExchangeVault {
                payer: keeper.pubkey(),
                store,
                vault,
                system_program: system_program::ID,
            },
            gmsol_store::instruction::PrepareGtExchangeVault {
                time_window_index: vault_index,
            },
        )],
        &[&keeper],
        "prepare gt exchange vault",
    )
    .await;

    send_ok(
        &mut ctx,
        &[ix(
            gmsol_treasury::ID,
            gmsol_treasury::accounts::PrepareGtBank {
                authority: keeper.pubkey(),
                store,
                config,
                treasury_vault_config,
                gt_exchange_vault: vault,
                gt_bank,
                store_program: gmsol_store::ID,
                system_program: system_program::ID,
            },
            gmsol_treasury::instruction::PrepareGtBank {},
        )],
        &[&keeper],
        "prepare gt bank",
    )
    .await;

    send_ok(
        &mut ctx,
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ix(
                gmsol_treasury::ID,
                gmsol_treasury::accounts::DepositToTreasuryVault {
                    authority: keeper.pubkey(),
                    store,
                    config,
                    treasury_vault_config,
                    receiver,
                    gt_exchange_vault: vault,
                    gt_bank,
                    token: payout_mint.pubkey(),
                    receiver_vault,
                    treasury_vault,
                    gt_bank_vault,
                    store_program: gmsol_store::ID,
                    token_program: anchor_spl::token::ID,
                    associated_token_program: anchor_spl::associated_token::ID,
                },
                gmsol_treasury::instruction::DepositToTreasuryVault {},
            ),
        ],
        &[&keeper],
        "deposit to treasury vault (fund gt bank)",
    )
    .await;

    // Fixture invariants: the whole BANK_FUNDS is now recorded in, and
    // held by, the GT bank (gt_factor == 100%).
    let bank = load_gt_bank(&mut ctx, &gt_bank).await;
    assert_eq!(bank.get_balance(&payout_mint.pubkey()), Some(BANK_FUNDS));
    assert_eq!(token_amount(&mut ctx, &gt_bank_vault).await, BANK_FUNDS);

    let exchange = gt_exchange_address(&vault, &owner.pubkey());

    Fixture {
        ctx,
        admin,
        keeper,
        owner,
        sponsor,
        delegate,
        store,
        config,
        treasury_vault_config,
        gt_bank,
        vault,
        vault_index,
        window,
        exchange,
        payout_mint: payout_mint.pubkey(),
        receiver_vault,
        treasury_vault,
        gt_bank_vault,
        owner_target,
    }
}

// ---------------------------------------------------------------------------
// Scenario building blocks
// ---------------------------------------------------------------------------

/// `request_gt_exchange_v2` with the given authority / rent payer.
fn request_v2_ix(f: &Fixture, authority: &Pubkey, rent_payer: &Pubkey) -> Instruction {
    ix(
        gmsol_store::ID,
        gmsol_store::accounts::RequestGtExchangeV2 {
            owner: f.owner.pubkey(),
            authority: *authority,
            payer: *rent_payer,
            store: f.store,
            user: user_address(&f.store, &f.owner.pubkey()),
            vault: f.vault,
            exchange: f.exchange,
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: gmsol_store::ID,
        },
        gmsol_store::instruction::RequestGtExchangeV2 {
            amount: EXCHANGE_AMOUNT,
        },
    )
}

/// Confirm the store-side exchange vault (standalone path used by the
/// repo's own anchor_test; buyback value 0).
fn confirm_vault_ix(f: &Fixture) -> Instruction {
    ix(
        gmsol_store::ID,
        gmsol_store::accounts::ConfirmGtExchangeVault {
            authority: f.keeper.pubkey(),
            store: f.store,
            vault: f.vault,
            event_authority: event_authority(),
            program: gmsol_store::ID,
        },
        gmsol_store::instruction::ConfirmGtExchangeVaultV2 {
            buyback_value: 0,
            buyback_price: None,
        },
    )
}

/// Treasury `complete_gt_exchange` with the single payout token laid out
/// as (mint, bank vault, target) remaining-account triples, exactly as
/// `CompleteGtExchange::execute` and the SDK builder lay them out.
fn complete_ix(f: &Fixture) -> Instruction {
    let remaining = vec![
        AccountMeta::new_readonly(f.payout_mint, false),
        AccountMeta::new(f.gt_bank_vault, false),
        AccountMeta::new(f.owner_target, false),
    ];
    ix_with_remaining(
        gmsol_treasury::ID,
        gmsol_treasury::accounts::CompleteGtExchange {
            owner: f.owner.pubkey(),
            store: f.store,
            config: f.config,
            treasury_vault_config: f.treasury_vault_config,
            gt_exchange_vault: f.vault,
            gt_bank: f.gt_bank,
            exchange: f.exchange,
            store_program: gmsol_store::ID,
            token_program: anchor_spl::token::ID,
            token_2022_program: anchor_spl::token_2022::ID,
        },
        gmsol_treasury::instruction::CompleteGtExchange {},
        remaining,
    )
}

fn close_v2_ix(f: &Fixture, authority: &Pubkey, rent_receiver: &Pubkey) -> Instruction {
    ix(
        gmsol_store::ID,
        gmsol_store::accounts::CloseGtExchangeV2 {
            authority: *authority,
            store: f.store,
            owner: f.owner.pubkey(),
            vault: f.vault,
            rent_receiver: *rent_receiver,
            exchange: f.exchange,
        },
        gmsol_store::instruction::CloseGtExchangeV2 {},
    )
}

struct Snapshot {
    exchange_lamports: u64,
    exchange_data: Vec<u8>,
    bank_vault_amount: u64,
    owner_target_amount: u64,
    sponsor_lamports: u64,
}

async fn snapshot(f: &mut Fixture) -> Snapshot {
    Snapshot {
        exchange_lamports: lamports(&mut f.ctx, &f.exchange).await,
        exchange_data: account_data(&mut f.ctx, &f.exchange)
            .await
            .unwrap_or_default(),
        bank_vault_amount: token_amount(&mut f.ctx, &f.gt_bank_vault).await,
        owner_target_amount: token_amount(&mut f.ctx, &f.owner_target).await,
        sponsor_lamports: lamports(&mut f.ctx, &f.sponsor.pubkey()).await,
    }
}

fn assert_unchanged(before: &Snapshot, after: &Snapshot) {
    assert_eq!(before.exchange_lamports, after.exchange_lamports);
    assert_eq!(before.exchange_data, after.exchange_data);
    assert_eq!(before.bank_vault_amount, after.bank_vault_amount);
    assert_eq!(before.owner_target_amount, after.owner_target_amount);
}

/// Shared tail of the freeze scenarios: request already placed, warp
/// past the window, confirm the vault, then prove that completion
/// reverts with `RentReceiverMismatched` and moves nothing.
async fn assert_freeze(f: &mut Fixture) {
    // Creation-time state: the exchange records the sponsor (not the
    // owner) as rent receiver — the trigger of the freeze.
    let exchange = load_gt_exchange(&mut f.ctx, &f.exchange).await;
    assert_eq!(exchange.amount(), EXCHANGE_AMOUNT);
    assert_eq!(*exchange.rent_receiver(), f.sponsor.pubkey());
    assert_eq!(*exchange.owner(), f.owner.pubkey());

    warp_past_window(&mut f.ctx, f.window).await;
    let confirm_ix = confirm_vault_ix(f);
    let keeper = f.keeper.insecure_clone();
    send_ok(
        &mut f.ctx,
        &[confirm_ix],
        &[&keeper],
        "confirm gt exchange vault",
    )
    .await;

    let before = snapshot(f).await;

    // THE PROOF: completion must fail inside the Treasury -> Store CPI
    // with the Store program's RentReceiverMismatched.
    let completion_ix = complete_ix(f);
    let owner = f.owner.insecure_clone();
    let err = send(
        &mut f.ctx,
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            completion_ix,
        ],
        &[&owner],
    )
    .await
    .expect_err("completion of a separate-payer exchange must fail");
    assert_eq!(
        custom_code(&err),
        Some(rent_receiver_mismatched_code()),
        "expected CoreError::RentReceiverMismatched, got {err:?}"
    );

    // Atomicity: nothing moved.
    let after = snapshot(f).await;
    assert_unchanged(&before, &after);
    let exchange = load_gt_exchange(&mut f.ctx, &f.exchange).await;
    assert_eq!(exchange.amount(), EXCHANGE_AMOUNT);
}

// ---------------------------------------------------------------------------
// Tier-1 tests
// ---------------------------------------------------------------------------

/// Freeze, variant A (minimal trigger): the owner signs the request
/// themselves, but a sponsor pays the rent.
#[tokio::test]
async fn freeze_variant_a_owner_authority_sponsor_payer() {
    let mut f = setup().await;
    let owner = f.owner.insecure_clone();
    let sponsor = f.sponsor.insecure_clone();
    let request_ix = request_v2_ix(&f, &owner.pubkey(), &sponsor.pubkey());
    send_ok(
        &mut f.ctx,
        &[request_ix],
        &[&owner, &sponsor],
        "request gt exchange v2 (variant A)",
    )
    .await;
    assert_freeze(&mut f).await;
}

/// Freeze, variant B (headline flow): a program-controlled owner that
/// never signs; its GT delegate places the request and a sponsor pays
/// the rent.
#[tokio::test]
async fn freeze_variant_b_delegate_authority_sponsor_payer() {
    let mut f = setup().await;
    let owner = f.owner.insecure_clone();
    let delegate = f.delegate.insecure_clone();
    let sponsor = f.sponsor.insecure_clone();

    // Owner records the delegate and an allowance covering the amount
    // (set_gt_delegate replaces both atomically).
    send_ok(
        &mut f.ctx,
        &[ix(
            gmsol_store::ID,
            gmsol_store::accounts::UpdateGtDelegation {
                owner: owner.pubkey(),
                store: f.store,
                user: user_address(&f.store, &owner.pubkey()),
                delegate: Some(delegate.pubkey()),
                event_authority: event_authority(),
                program: gmsol_store::ID,
            },
            gmsol_store::instruction::SetGtDelegate {
                allowance: EXCHANGE_AMOUNT,
            },
        )],
        &[&owner],
        "set gt delegate",
    )
    .await;

    let request_ix = request_v2_ix(&f, &delegate.pubkey(), &sponsor.pubkey());
    send_ok(
        &mut f.ctx,
        &[request_ix],
        &[&delegate, &sponsor],
        "request gt exchange v2 (variant B)",
    )
    .await;
    assert_freeze(&mut f).await;
}

/// Control (Tier-1 form): identical in every way except
/// `rent_payer == owner`. The V1 close inside `complete_gt_exchange`
/// must now PASS — the transaction instead fails later, at the bank
/// accounting precondition (`remaining_confirmed_gt_amount == 0` in
/// Tier 1, which only `confirm_gt_buyback` sets), with
/// `CoreError::Internal`. The rent-receiver check is thereby isolated
/// as the single differing gate. The full successful payout is proven
/// by the Tier-2 test below.
#[tokio::test]
async fn control_owner_payer_passes_the_rent_receiver_gate() {
    let mut f = setup().await;
    let owner = f.owner.insecure_clone();
    let request_ix = request_v2_ix(&f, &owner.pubkey(), &owner.pubkey());
    send_ok(
        &mut f.ctx,
        &[request_ix],
        &[&owner],
        "request gt exchange v2 (control)",
    )
    .await;

    let exchange = load_gt_exchange(&mut f.ctx, &f.exchange).await;
    assert_eq!(*exchange.rent_receiver(), f.owner.pubkey());

    warp_past_window(&mut f.ctx, f.window).await;
    let keeper = f.keeper.insecure_clone();
    let confirm_ix = confirm_vault_ix(&f);
    send_ok(
        &mut f.ctx,
        &[confirm_ix],
        &[&keeper],
        "confirm gt exchange vault (control)",
    )
    .await;

    let before = snapshot(&mut f).await;
    let completion_ix = complete_ix(&f);
    let err = send(
        &mut f.ctx,
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            completion_ix,
        ],
        &[&owner],
    )
    .await
    .expect_err("tier-1 control stops at the unconfirmed bank");
    let code = custom_code(&err).expect("custom program error");
    assert_ne!(
        code,
        rent_receiver_mismatched_code(),
        "control case must NOT hit the rent-receiver gate"
    );
    assert_eq!(
        code,
        u32::from(gmsol_store::CoreError::Internal),
        "control case should fail at require_gte!(remaining_confirmed, gt_amount)"
    );
    // Atomic revert: the successful close inside the tx is rolled back.
    let after = snapshot(&mut f).await;
    assert_unchanged(&before, &after);
}

/// No-rescue probe (a): the owner cannot close their own frozen
/// exchange via `close_gt_exchange_v2` — it is GT-controller-gated
/// (mirrors the repo anchor_test's "the owner is not a GT controller").
#[tokio::test]
async fn no_rescue_owner_cannot_close_v2() {
    let mut f = setup().await;
    let owner = f.owner.insecure_clone();
    let sponsor = f.sponsor.insecure_clone();
    let request_ix = request_v2_ix(&f, &owner.pubkey(), &sponsor.pubkey());
    send_ok(
        &mut f.ctx,
        &[request_ix],
        &[&owner, &sponsor],
        "request gt exchange v2 (no-rescue a)",
    )
    .await;
    warp_past_window(&mut f.ctx, f.window).await;
    let keeper = f.keeper.insecure_clone();
    let confirm_ix = confirm_vault_ix(&f);
    send_ok(
        &mut f.ctx,
        &[confirm_ix],
        &[&keeper],
        "confirm vault (no-rescue a)",
    )
    .await;

    let before = snapshot(&mut f).await;
    let sponsor_pk = f.sponsor.pubkey();
    let close_ix = close_v2_ix(&f, &owner.pubkey(), &sponsor_pk);
    send(&mut f.ctx, &[close_ix], &[&owner])
        .await
        .expect_err("owner is not a GT controller");
    let after = snapshot(&mut f).await;
    assert_unchanged(&before, &after);
}

/// No-rescue probe (b), destructive — separate instance: a GT
/// controller CAN close the frozen exchange via V2, but that pays out
/// nothing: the rent goes to the recorded sponsor, the bank keeps its
/// tokens, the owner receives nothing, and the entitlement record is
/// gone, so completion afterwards fails for a different reason
/// (missing account). The only privileged interaction is a non-rescue.
#[tokio::test]
async fn no_rescue_controller_v2_close_pays_nothing() {
    let mut f = setup().await;
    let owner = f.owner.insecure_clone();
    let sponsor = f.sponsor.insecure_clone();
    let request_ix = request_v2_ix(&f, &owner.pubkey(), &sponsor.pubkey());
    send_ok(
        &mut f.ctx,
        &[request_ix],
        &[&owner, &sponsor],
        "request gt exchange v2 (no-rescue b)",
    )
    .await;
    warp_past_window(&mut f.ctx, f.window).await;
    let keeper = f.keeper.insecure_clone();
    let confirm_ix = confirm_vault_ix(&f);
    send_ok(
        &mut f.ctx,
        &[confirm_ix],
        &[&keeper],
        "confirm vault (no-rescue b)",
    )
    .await;

    let before = snapshot(&mut f).await;
    let sponsor_pk = f.sponsor.pubkey();
    let close_ix = close_v2_ix(&f, &keeper.pubkey(), &sponsor_pk);
    send_ok(
        &mut f.ctx,
        &[close_ix],
        &[&keeper],
        "controller close_gt_exchange_v2",
    )
    .await;

    // Exchange deleted; its lamports (rent) went to the sponsor.
    assert!(account_data(&mut f.ctx, &f.exchange).await.is_none());
    let sponsor_after = lamports(&mut f.ctx, &f.sponsor.pubkey()).await;
    assert_eq!(
        sponsor_after,
        before.sponsor_lamports + before.exchange_lamports,
        "rent must go to the recorded rent receiver (the sponsor)"
    );
    // Nothing was paid out.
    assert_eq!(token_amount(&mut f.ctx, &f.owner_target).await, 0);
    assert_eq!(
        token_amount(&mut f.ctx, &f.gt_bank_vault).await,
        before.bank_vault_amount
    );

    // Completion is now impossible for the trivial reason that the
    // entitlement record no longer exists.
    let completion_ix = complete_ix(&f);
    send(
        &mut f.ctx,
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            completion_ix,
        ],
        &[&owner],
    )
    .await
    .expect_err("exchange record is gone");
}

// ---------------------------------------------------------------------------
// Tier-2 test (ignored): full positive control with a confirmed bank.
//
// `confirm_gt_buyback` is the only instruction that sets the bank's
// `remaining_confirmed_gt_amount`. It is oracle-priced: it CPIs to the
// Store's `set_prices_from_price_feed` using the token map's feed for
// the payout token. Wiring a price feed that the Store accepts is the
// one fixture piece that cannot be derived from the GT/treasury sources
// alone — the repo's anchor_test does it with the mock Chainlink
// verifier program plus devnet-cloned feed accounts. The fixture
// helper below is therefore marked TODO-VERIFY; the rest of the test
// is complete: with a confirmed bank, the owner-payer control case
// must complete and pay out, while (by the Tier-1 tests) the identical
// separate-payer case still reverts at the close CPI.
// ---------------------------------------------------------------------------

/// TODO-VERIFY: create the Store token map + oracle and register a
/// price feed for `mint` that `set_prices_from_price_feed` accepts,
/// returning (token_map, oracle, feed_account). Reference
/// implementations: `tests/anchor_test/setup.rs`
/// (`initialize_token_map`, `initialize_mock`, token-config insertion)
/// and the `ConfirmGtBuybackBuilder` in
/// `crates/sdk/src/client/ops/treasury.rs` (feed selection / ordering).
async fn setup_oracle_and_feed(
    _f: &mut Fixture,
    _mint: &Pubkey,
) -> (Pubkey, Pubkey, Pubkey) {
    unimplemented!(
        "TODO-VERIFY: wire token map + oracle + price feed (see README, Tier 2)"
    )
}

#[tokio::test]
#[ignore = "requires the oracle price-feed fixture (TODO-VERIFY, see README)"]
async fn control_owner_payer_full_payout_with_confirmed_bank() {
    let mut f = setup().await;
    let payout_mint = f.payout_mint;
    let (token_map, oracle, feed) = setup_oracle_and_feed(&mut f, &payout_mint).await;

    let owner = f.owner.insecure_clone();
    let request_ix = request_v2_ix(&f, &owner.pubkey(), &owner.pubkey());
    send_ok(
        &mut f.ctx,
        &[request_ix],
        &[&owner],
        "request gt exchange v2 (tier-2 control)",
    )
    .await;

    warp_past_window(&mut f.ctx, f.window).await;

    // Confirm the bank (and, via its CPI, the store vault). Remaining
    // accounts per ConfirmGtBuyback's doc: [feeds..., treasury
    // mints..., treasury vaults...]; with the single payout token that
    // is [feed, payout_mint, treasury_vault].
    let keeper = f.keeper.insecure_clone();
    let confirm_buyback = ix_with_remaining(
        gmsol_treasury::ID,
        gmsol_treasury::accounts::ConfirmGtBuyback {
            authority: keeper.pubkey(),
            store: f.store,
            config: f.config,
            treasury_vault_config: f.treasury_vault_config,
            gt_exchange_vault: f.vault,
            gt_bank: f.gt_bank,
            token_map,
            oracle,
            event_authority: event_authority(),
            store_program: gmsol_store::ID,
            chainlink_program: None,
        },
        gmsol_treasury::instruction::ConfirmGtBuyback {},
        vec![
            AccountMeta::new_readonly(feed, false),
            AccountMeta::new_readonly(f.payout_mint, false),
            AccountMeta::new(f.treasury_vault, false),
        ],
    );
    send_ok(
        &mut f.ctx,
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            confirm_buyback,
        ],
        &[&keeper],
        "confirm gt buyback",
    )
    .await;

    let bank = load_gt_bank(&mut f.ctx, &f.gt_bank).await;
    assert!(bank.is_confirmed(), "bank must be confirmed");
    // NOTE: `remaining_confirmed_gt_amount()` is `pub(crate)` in
    // gmsol-treasury, so it cannot be asserted directly from an
    // integration test; the payout assertions below pin it
    // behaviourally (full pro-rata payout for the single claimer, and
    // no second completion possible).

    let exchange_lamports_before = lamports(&mut f.ctx, &f.exchange).await;
    let owner_lamports_before = lamports(&mut f.ctx, &f.owner.pubkey()).await;
    let bank_vault_before = token_amount(&mut f.ctx, &f.gt_bank_vault).await;

    let completion_ix = complete_ix(&f);
    send_ok(
        &mut f.ctx,
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            completion_ix,
        ],
        &[&owner],
        "complete gt exchange (tier-2 control)",
    )
    .await;

    // Paid out, exchange closed, rent refunded to the owner.
    let payout = token_amount(&mut f.ctx, &f.owner_target).await;
    assert!(payout > 0, "control case must pay out");
    assert_eq!(
        token_amount(&mut f.ctx, &f.gt_bank_vault).await,
        bank_vault_before - payout,
        "payout must come out of the GT bank vault"
    );
    assert!(account_data(&mut f.ctx, &f.exchange).await.is_none());
    assert_eq!(
        lamports(&mut f.ctx, &f.owner.pubkey()).await,
        owner_lamports_before + exchange_lamports_before,
        "exchange rent must be refunded to the owner"
    );
}
