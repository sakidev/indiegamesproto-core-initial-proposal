use borsh::BorshSerialize;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

use crate::{
    constants::{ORACLE_DISCRIMINATOR, PROGRAM_ID},
    helpers::{assert_authority, find_oracle_pda, load_oracle},
    state::oracle::OraclePriceState
};



/// Initialize the singleton oracle PDA (just need to be called once after the program is firstly deployed).
///
/// Accounts:
///   0. `[signer, writable]` Authority
///   1. `[writable]`         Oracle PDA  (seeds: ["igp_oracle"])
///   2. `[]`                 System program
pub fn init_oracle(
    accounts: &[AccountInfo],
    sol_price_usd_micro_per_lamport: u64,
    skr_price_usd_micro_per_atom: u64,
    slice_price_usd_micro_per_atom: u64,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let authority = next_account_info(iter)?;
    let oracle_acct = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    assert_authority(authority)?;

    let (expected_pda, bump) = find_oracle_pda();
    if *oracle_acct.key != expected_pda {
        msg!("InitOracle: invalid oracle PDA (expected {}, got {})", expected_pda, oracle_acct.key);
        return Err(ProgramError::InvalidAccountData);
    }
    if !oracle_acct.data_is_empty() {
        msg!("InitOracle: oracle account already initialized");
        return Err(ProgramError::AccountAlreadyInitialized);
    }

    if sol_price_usd_micro_per_lamport == 0
        || skr_price_usd_micro_per_atom == 0
        || slice_price_usd_micro_per_atom == 0
    {
        msg!("InitOracle: oracle prices must be greater than zero");
        return Err(ProgramError::InvalidInstructionData);
    }

    let bump_slice = [bump];
    let signer_seeds: &[&[u8]] = &[b"igp_oracle", &bump_slice];
    let lamports = Rent::get()?.minimum_balance(OraclePriceState::LEN);

    invoke_signed(
        &system_instruction::create_account(
            authority.key,
            oracle_acct.key,
            lamports,
            OraclePriceState::LEN as u64,
            &PROGRAM_ID,
        ),
        &[authority.clone(), oracle_acct.clone(), system_program.clone()],
        &[signer_seeds],
    )?;

    OraclePriceState {
        discriminator: ORACLE_DISCRIMINATOR,
        sol_price_usd_micro_per_lamport,
        skr_price_usd_micro_per_atom,
        slice_price_usd_micro_per_atom,
        bump,
    }
    .serialize(&mut &mut oracle_acct.data.borrow_mut()[..])?;

    msg!(
        "Oracle initialized | sol_micro={} skr_micro={} slice_micro={}",
        sol_price_usd_micro_per_lamport,
        skr_price_usd_micro_per_atom,
        slice_price_usd_micro_per_atom
    );

    Ok(())
}

/// Update all three prices in the global oracle PDA.
///
/// Only AUTHORITY may call this. The next join after this call will
/// automatically use the new rates. Users cannot influence prices.
///
/// Accounts:
///   0. `[signer]`   Authority
///   1. `[writable]` Oracle PDA
pub fn update_oracle_prices(
    accounts: &[AccountInfo],
    sol_price_usd_micro_per_lamport: u64,
    skr_price_usd_micro_per_atom: u64,
    slice_price_usd_micro_per_atom: u64,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let authority = next_account_info(iter)?;
    let oracle_acct = next_account_info(iter)?;

    assert_authority(authority)?;

    if sol_price_usd_micro_per_lamport == 0
        || skr_price_usd_micro_per_atom == 0
        || slice_price_usd_micro_per_atom == 0
    {
        msg!("UpdateOraclePrices: oracle prices must be greater than zero");
        return Err(ProgramError::InvalidInstructionData);
    }

    // load_oracle validates ownership, PDA derivation, and discriminator.
    let mut state = load_oracle(oracle_acct)?;
    state.sol_price_usd_micro_per_lamport = sol_price_usd_micro_per_lamport;
    state.skr_price_usd_micro_per_atom = skr_price_usd_micro_per_atom;
    state.slice_price_usd_micro_per_atom = slice_price_usd_micro_per_atom;
    state.serialize(&mut &mut oracle_acct.data.borrow_mut()[..])?;

    msg!(
        "Oracle updated | sol_micro={} skr_micro={} slice_micro={}",
        sol_price_usd_micro_per_lamport,
        skr_price_usd_micro_per_atom,
        slice_price_usd_micro_per_atom,
    );
    Ok(())
}