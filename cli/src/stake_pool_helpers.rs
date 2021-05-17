use solana_program::sysvar;

use {
    crate::helpers::{check_fee_payer_balance, send_transaction},
    crate::{CommandResult, Config},
    solana_program::{borsh::get_packed_len, program_pack::Pack, pubkey::Pubkey},
    solana_sdk::{
        signature::{Keypair, Signer},
        system_instruction,
        transaction::Transaction,
    },
    spl_stake_pool::{
        self,
        borsh::get_instance_packed_len,
        find_withdraw_authority_program_address,
        stake_program::{self},
        state::{Fee, StakePool, ValidatorList},
    },
};

const STAKE_STATE_LEN: usize = 200;

pub(crate) fn command_create_pool(
    config: &Config,
    deposit_authority: &Pubkey,
    fee: Fee,
    max_validators: u32,
    stake_pool_keypair: Option<Keypair>,
    mint_keypair: Option<Keypair>,
) -> CommandResult {
    let reserve_stake = Keypair::new();
    println!(
        "Creating stake pool reserve stake {}",
        reserve_stake.pubkey()
    );

    let mint_keypair = mint_keypair.unwrap_or_else(Keypair::new);
    println!("Creating stake pool mint {}", mint_keypair.pubkey());

    let pool_fee_account = Keypair::new();
    println!(
        "Creating stake pool fee collection account {}",
        pool_fee_account.pubkey()
    );

    let stake_pool_keypair = stake_pool_keypair.unwrap_or_else(Keypair::new);

    let validator_list = Keypair::new();

    let reserve_stake_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(STAKE_STATE_LEN)?
        + 1;
    let mint_account_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN)?;
    let pool_fee_account_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(spl_token::state::Account::LEN)?;
    let stake_pool_account_lamports = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(get_packed_len::<StakePool>())?;
    let empty_validator_list = ValidatorList::new(max_validators);
    let validator_list_size = get_instance_packed_len(&empty_validator_list)?;
    let validator_list_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(validator_list_size)?;
    let total_rent_free_balances = reserve_stake_balance
        + mint_account_balance
        + pool_fee_account_balance
        + stake_pool_account_lamports
        + validator_list_balance;

    let default_decimals = spl_token::native_mint::DECIMALS;

    // Calculate withdraw authority used for minting pool tokens
    let (withdraw_authority, _) = find_withdraw_authority_program_address(
        &spl_stake_pool::id(),
        &stake_pool_keypair.pubkey(),
    );

    if config.verbose {
        println!("Stake pool withdraw authority {}", withdraw_authority);
    }

    let mut setup_transaction = Transaction::new_with_payer(
        &[
            // Account for the stake pool reserve
            system_instruction::create_account(
                &config.fee_payer.pubkey(),
                &reserve_stake.pubkey(),
                reserve_stake_balance,
                STAKE_STATE_LEN as u64,
                &stake_program::id(),
            ),
            stake_program::initialize(
                &reserve_stake.pubkey(),
                &stake_program::Authorized {
                    staker: withdraw_authority,
                    withdrawer: withdraw_authority,
                },
                &stake_program::Lockup::default(),
            ),
            // Account for the stake pool mint
            system_instruction::create_account(
                &config.fee_payer.pubkey(),
                &mint_keypair.pubkey(),
                mint_account_balance,
                spl_token::state::Mint::LEN as u64,
                &spl_token::id(),
            ),
            // Account for the pool fee accumulation
            system_instruction::create_account(
                &config.fee_payer.pubkey(),
                &pool_fee_account.pubkey(),
                pool_fee_account_balance,
                spl_token::state::Account::LEN as u64,
                &spl_token::id(),
            ),
            // Initialize pool token mint account
            spl_token::instruction::initialize_mint(
                &spl_token::id(),
                &mint_keypair.pubkey(),
                &withdraw_authority,
                None,
                default_decimals,
            )?,
            // Initialize fee receiver account
            spl_token::instruction::initialize_account(
                &spl_token::id(),
                &pool_fee_account.pubkey(),
                &mint_keypair.pubkey(),
                &config.manager.pubkey(),
            )?,
        ],
        Some(&config.fee_payer.pubkey()),
    );

    let mut initialize_transaction = Transaction::new_with_payer(
        &[
            // Validator stake account list storage
            system_instruction::create_account(
                &config.fee_payer.pubkey(),
                &validator_list.pubkey(),
                validator_list_balance,
                validator_list_size as u64,
                &spl_stake_pool::id(),
            ),
            // Account for the stake pool
            system_instruction::create_account(
                &config.fee_payer.pubkey(),
                &stake_pool_keypair.pubkey(),
                stake_pool_account_lamports,
                get_packed_len::<StakePool>() as u64,
                &spl_stake_pool::id(),
            ),
            // Initialize stake pool
            lido::instruction::initialize_stake_pool_with_authority(
                &spl_stake_pool::id(),
                &lido::instruction::InitializeStakePoolWithAuthorityAccountsMeta {
                    stake_pool: stake_pool_keypair.pubkey(),
                    manager: config.manager.pubkey(),
                    staker: config.staker.pubkey(),
                    validator_list: validator_list.pubkey(),
                    reserve_stake: reserve_stake.pubkey(),
                    pool_mint: mint_keypair.pubkey(),
                    manager_pool_account: pool_fee_account.pubkey(),
                    deposit_authority: *deposit_authority,
                    sysvar_clock: sysvar::clock::id(),
                    sysvar_rent: sysvar::rent::id(),
                    sysvar_token: spl_token::id(),
                },
                fee,
                max_validators,
            )?,
        ],
        Some(&config.fee_payer.pubkey()),
    );

    let (recent_blockhash, fee_calculator) = config.rpc_client.get_recent_blockhash()?;
    check_fee_payer_balance(
        config,
        total_rent_free_balances
            + fee_calculator.calculate_fee(&setup_transaction.message())
            + fee_calculator.calculate_fee(&initialize_transaction.message()),
    )?;
    let setup_signers = vec![
        config.fee_payer.as_ref(),
        &mint_keypair,
        &pool_fee_account,
        &reserve_stake,
    ];
    setup_transaction.sign(&setup_signers, recent_blockhash);
    send_transaction(&config, setup_transaction)?;

    let initialize_signers = vec![
        config.fee_payer.as_ref(),
        &stake_pool_keypair,
        &validator_list,
        config.manager.as_ref(),
    ];
    initialize_transaction.sign(&initialize_signers, recent_blockhash);
    send_transaction(&config, initialize_transaction)?;
    Ok(())
}