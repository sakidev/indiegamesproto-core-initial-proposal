use borsh::BorshDeserialize;
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, msg, program_error::ProgramError,
    pubkey::Pubkey,
};

use crate::{
    constants::*,
    state::oracle::OraclePriceState,
    state::game::GAME_TITLE_MAX_LEN,
};

// -- PDA Derivations ------------------------------------------
#[inline]
pub fn find_oracle_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"igp_oracle"], &PROGRAM_ID)
}

#[inline]
pub fn find_game_pda(game_title: &[u8; GAME_TITLE_MAX_LEN]) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"igp_game", game_title], &PROGRAM_ID)
}

#[inline]
pub fn find_user_account_pda(owner: &Pubkey, game: &Pubkey, timestamp: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"igp_user", owner.as_ref(), game.as_ref(), &timestamp.to_le_bytes()],
        &PROGRAM_ID,
    )
}

#[inline]
pub fn find_game_protocol_pda(game_title: &[u8; GAME_TITLE_MAX_LEN]) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"igp_game_protocol", game_title], &PROGRAM_ID)
}

#[inline]
pub fn find_ranked_game_pda(ranked_game_id: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"igp_ranked_game", &ranked_game_id.to_le_bytes()], &PROGRAM_ID)
}

// -- USD -> Token conversion ------------------------------------------

/// Convert USD cents to token base-units, using the oracle price.
///
/// `price_micro_usd_per_token` - the oracle field (micro-USD per whole token).
/// `decimals`                  - token's decimal precision (9 for SOL, 6 for SPL).
///
///   base_units = ceil( (usd_cents × 10_000 × 10^decimals) / price_micro_usd )
pub fn usd_cents_to_base_units(
    usd_cents: u64,
    price_micro_usd_per_token: u64,
    decimals: u8
) -> Result<u64, ProgramError> {
    if price_micro_usd_per_token == 0 {
        msg!("Oracle price is zero");
        return Err(ProgramError::InvalidAccountData);
    }
    let base: u128 = 10_u128.pow(decimals as u32);
    let numerator = (usd_cents as u128)
        .checked_mul(10_000)
        .ok_or(ProgramError::ArithmeticOverflow)?
        .checked_mul(base)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let denominator = price_micro_usd_per_token as u128;
    let result = numerator
        .checked_add(denominator - 1)
        .ok_or(ProgramError::ArithmeticOverflow)?
        .checked_div(denominator)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if result > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    }
    Ok(result as u64)
}

// -- Oracle Loader ------------------------------------------------

/// Load, validate, and deserialize the oracle PDA.
///
/// Checks:
///   1. `owner == PROGRAM_ID`              - not a user-supplied fake
///   2. `key   == find_oracle_pda()`       - canonical derivation
///   3. `discriminator == ORACLE_DISCRIMINATOR`
pub fn load_oracle(oracle_acct: &AccountInfo) -> Result<OraclePriceState, ProgramError> {
    if *oracle_acct.owner != PROGRAM_ID {
        msg!("Oracle account not owned by this program");
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expected_pda, _) = find_oracle_pda();
    if *oracle_acct.key != expected_pda {
        msg!(
            "Oracle PDA mismatch: expected {} got {}",
            expected_pda,
            oracle_acct.key
        );
        return Err(ProgramError::InvalidSeeds);
    }
    let state = OraclePriceState::try_from_slice(&oracle_acct.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if state.discriminator != ORACLE_DISCRIMINATOR {
        msg!("Oracle account has wrong discriminator");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(state)
}

// -- FEE ARITHMETICS ------------------------------------------------

/// Generic basis-point calculation with **ceiling** division.
///   result = ceil(amount × bps / 10_000)
#[inline]
fn fee_ceil(amount: u64, bps: u16) -> u64 {
    ((amount as u128)
        .saturating_mul(bps as u128)
        .saturating_add(9_999)
        / 10_000) as u64
}

/// Generic basis-point calculation with **floor** division.
///   result = floor(amount × bps / 10_000)
#[inline]
fn fee_floor(amount: u64, bps: u16) -> u64 {
    ((amount as u128).saturating_mul(bps as u128) / 10_000) as u64
}

/// Compute the dynamic % protocol fee on an entry amount (ceiling division).
///
/// This is the total fee charged to the user; it is then split between the
/// game PDA and the protocol treasury via `split_protocol_fee`.
#[inline]
pub fn protocol_fee(game_fee_amount: u16, entry_amount: u64) -> u64 {
    fee_ceil(entry_amount, game_fee_amount)
}

/// Split a protocol fee amount between the game developer and the protocol treasury.
///
/// `share_to_protocol` - the protocol's configured share of the fee, in bps
/// (e.g. 2000 for 20 %). The game developer keeps the remainder.
///
/// Returns `(dev_amount, protocol_amount)` which always sum to `total_fee`.
///
/// Floor division is used for the protocol share so that any remainder (dust)
/// is kept by the game developer rather than being created from nothing.
#[inline]
pub fn split_protocol_fee(total_fee: u64, share_to_protocol: u16) -> (u64, u64) {
    let protocol_amount = fee_floor(total_fee, share_to_protocol);
    let dev_amount = total_fee.saturating_sub(protocol_amount);
    (dev_amount, protocol_amount)
}

/// Validate that `dev_fee_bps` is within the allowed range
/// [MIN_GAME_TO_PROTOCOL_SHARE_BPS, 10_000].
#[inline]
pub fn validate_dev_fee_bps(dev_fee_bps: u16) -> ProgramResult {
    if dev_fee_bps < MIN_GAME_TO_PROTOCOL_SHARE_BPS {
        msg!(
            "dev_fee_bps {} is below the protocol minimum of {}",
            dev_fee_bps,
            MIN_GAME_TO_PROTOCOL_SHARE_BPS
        );
        return Err(ProgramError::InvalidInstructionData);
    }
    if dev_fee_bps > 10_000 {
        msg!("dev_fee_bps {} exceeds 10_000 (100 %)", dev_fee_bps);
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok(())
}

/// Hardcoded token decimals (SOL native = 9, all supported SPL mints = 6).
#[inline]
pub fn token_decimals(token: &PaymentToken) -> u8 {
    match token {
        PaymentToken::SOL => 9,
        PaymentToken::SKR | PaymentToken::SLICE | PaymentToken::USDC => 6,
    }
}

/// Ranked entry fee on top is a fixed 10%.
pub const RANKED_FEE_BPS: u64 = 1000; // 10.00%
pub const BPS_DENOMINATOR: u64 = 10_000;


/// -- Authority guard ------------------------------------------------
pub fn assert_authority(account: &AccountInfo) -> ProgramResult {
    if account.key != &AUTHORITY {
        msg!("Error: Invalid authority");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}