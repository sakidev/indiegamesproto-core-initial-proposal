use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
    pubkey,
    pubkey::Pubkey,
};
use spl_associated_token_account::instruction::create_associated_token_account;
use spl_token::instruction::transfer_checked;

use crate::{
    constants::{PROGRAM_ID, PaymentToken, SKR_MINT, SLICE_MINT, USDC_MINT, AUTHORITY, RANKED_GAME_DISCRIMINATOR},
    helpers::{find_ranked_game_pda, usd_cents_to_base_units, split_protocol_fee, load_oracle, token_decimals, protocol_fee, find_game_protocol_pda, RANKED_FEE_BPS, BPS_DENOMINATOR},
    state::{
        game::GameAccount,
        ranked_game::{RankedGameState, RankedGameStatus},
        user_account::UserAccount,
    },
    processor::payment::{process_ranked_entry_payment, RankedSplPaymentAccounts},
    processor::oracle::price_for,
};

// ----------------------------------------------------------
// CREATE RANKED GAME
// ----------------------------------------------------------
pub fn create_ranked_game<'a>(
    accounts: &'a [AccountInfo<'a>],
    ranked_game_id: u64,
    entry_fee: u64,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?; // creator, signer, rent payer
    let ranked_game_account = next_account_info(iter)?; // PDA (state + SOL escrow)
    let game_account = next_account_info(iter)?; // parent game PDA
    let system_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let ata_program = next_account_info(iter)?; // SPL Associated Token Account program
    let rent_sysvar = next_account_info(iter)?; // rent sysvar account (for ATA CPI)

    // The 3 mints + their (to-be-created) escrow ATAs owned by the ranked game PDA.
    let usdc_mint = next_account_info(iter)?;
    let usdc_escrow_ata = next_account_info(iter)?;
    let skr_mint = next_account_info(iter)?;
    let skr_escrow_ata = next_account_info(iter)?;
    let slice_mint = next_account_info(iter)?;
    let slice_escrow_ata = next_account_info(iter)?;

    if !owner.is_signer {
        msg!("Owner must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    if *owner.key != AUTHORITY {
        msg!("Only the protocol authority can create ranked games");
        return Err(ProgramError::InvalidAccountData);
    }

    if !ranked_game_account.data_is_empty() {
        msg!("Ranked game account already initialized");
        return Err(ProgramError::AccountAlreadyInitialized);
    }

    // Validate the parent game.
    let game_data = {
        let mut slice: &[u8] = &game_account.data.borrow();
        GameAccount::deserialize(&mut slice)?
    };
    if !game_data.is_initialized {
        msg!("Associated game account is not initialized");
        return Err(ProgramError::InvalidAccountData);
    }

    // Validate the supplied mints are exactly the 3 supported SPL tokens.
    if *usdc_mint.key != USDC_MINT {
        msg!("Mint mismatch: expected USDC mint");
        return Err(ProgramError::InvalidAccountData);
    }
    if *skr_mint.key != SKR_MINT {
        msg!("Mint mismatch: expected SKR mint");
        return Err(ProgramError::InvalidAccountData);
    }
    if *slice_mint.key != SLICE_MINT {
        msg!("Mint mismatch: expected SLICE mint");
        return Err(ProgramError::InvalidAccountData);
    }

    let (pda, bump) = find_ranked_game_pda(ranked_game_id);
    if pda != *ranked_game_account.key {
        msg!("Ranked game account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- 1. Create the ranked game state / SOL escrow account ----------------
    let rent = Rent::get()?;
    let required_lamports = rent.minimum_balance(RankedGameState::LEN);

    invoke_signed(
        &system_instruction::create_account(
            owner.key,
            ranked_game_account.key,
            required_lamports,
            RankedGameState::LEN as u64,
            &PROGRAM_ID,
        ),
        &[owner.clone(), ranked_game_account.clone(), system_program.clone()],
        &[&[b"igp_ranked_game", &ranked_game_id.to_le_bytes(), &[bump]]],
    )?;

    // -- 2. Create the 3 escrow ATAs owned by the ranked game PDA ----------------
    // Net entry fees in each SPL token pool here for later winner payouts.
    // ATA addresses are deterministic (owner = ranked_game_pda, + mint), so the
    // PDA does not need to sign - the rent payer (owner) signs.
    create_escrow_ata(
        owner,
        ranked_game_account,
        usdc_mint,
        usdc_escrow_ata,
        system_program,
        token_program,
        ata_program,
        rent_sysvar,
        "USDC",
    )?;
    create_escrow_ata(
        owner,
        ranked_game_account,
        skr_mint,
        skr_escrow_ata,
        system_program,
        token_program,
        ata_program,
        rent_sysvar,
        "SKR",
    )?;
    create_escrow_ata(
        owner,
        ranked_game_account,
        slice_mint,
        slice_escrow_ata,
        system_program,
        token_program,
        ata_program,
        rent_sysvar,
        "SLICE",
    )?;

    // -- 4. Persist state ------------------------------------------------------
    let ranked_game_data = RankedGameState {
        discriminator: RANKED_GAME_DISCRIMINATOR,
        ranked_game_id,
        game: *game_account.key,
        entry_fee,
        participant_count: 0,
        status: RankedGameStatus::Open,
        bump,
    };
    ranked_game_data.serialize(&mut &mut ranked_game_account.data.borrow_mut()[..])?;

    msg!(
        "Ranked game created for game {} | entry {} (USD cents) | 3 escrow ATAs (USDC/SKR/SLICE) opened",
        game_account.key,
        entry_fee
    );

    Ok(())
}

// ----------------------------------------------------------
// Helper: create one escrow ATA owned by the ranked game PDA
// ----------------------------------------------------------
#[allow(clippy::too_many_arguments)]
fn create_escrow_ata<'a>(
    payer: &AccountInfo<'a>,
    ranked_game_pda: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    escrow_ata: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    ata_program: &AccountInfo<'a>,
    rent_sysvar: &AccountInfo<'a>,
    label: &str,
) -> ProgramResult {
    // Idempotent guard: skip if somehow already created.
    if !escrow_ata.data_is_empty() {
        msg!("{} escrow ATA already exists, skipping", label);
        return Ok(());
    }

    // Verify the passed account is the canonical ATA for (ranked_game_pda, mint).
    let expected_ata = spl_associated_token_account::get_associated_token_address(
        ranked_game_pda.key,
        mint.key,
    );
    if expected_ata != *escrow_ata.key {
        msg!("{} escrow ATA address mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }

    invoke(
        &create_associated_token_account(
            payer.key,            // funding account
            ranked_game_pda.key,  // wallet / owner of the new ATA
            mint.key,
            token_program.key,
        ),
        &[
            payer.clone(),
            escrow_ata.clone(),
            ranked_game_pda.clone(),
            mint.clone(),
            system_program.clone(),
            token_program.clone(),
            ata_program.clone(),
            rent_sysvar.clone(),
        ],
    )?;

    msg!("{} escrow ATA created for ranked game PDA", label);
    Ok(())
}

pub fn join_ranked_game<'a>(
    accounts: &'a [AccountInfo<'a>],
    ranked_game_id: u64,
    payment_token: Pubkey,
) -> ProgramResult {
    let iter = &mut accounts.iter();

    let player = next_account_info(iter)?;              // signer, payer
    let ranked_game_account = next_account_info(iter)?; // PDA (state + SOL escrow)
    let game_account = next_account_info(iter)?;        // parent game PDA (dev cut)
    let game_protocol = next_account_info(iter)?;       // game protocol PDA (protocol cut)
    let oracle_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    // SPL-only accounts (ignored on the SOL path).
    let mint = next_account_info(iter)?;
    let payer_ata = next_account_info(iter)?;
    let ranked_escrow_ata = next_account_info(iter)?;
    let game_ata = next_account_info(iter)?;
    let protocol_ata = next_account_info(iter)?;

    // User account (caller's per-game account); updated to point at this ranked game.
    let user_account = next_account_info(iter)?;

    if !player.is_signer {
        msg!("Player must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // -- Validate ranked game state ------------------------------------------------
    if ranked_game_account.owner != &PROGRAM_ID {
        msg!("Ranked game account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let (pda, _bump) = find_ranked_game_pda(ranked_game_id);
    if pda != *ranked_game_account.key {
        msg!("Ranked game account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let mut ranked_game_data = RankedGameState::try_from_slice(
        &ranked_game_account.data.borrow(),
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;

    if ranked_game_data.discriminator != RANKED_GAME_DISCRIMINATOR {
        msg!("Invalid ranked game discriminator");
        return Err(ProgramError::InvalidAccountData);
    }
    if ranked_game_data.status != RankedGameStatus::Open {
        msg!("Ranked game is not open for joining");
        return Err(ProgramError::InvalidAccountData);
    }
    if ranked_game_data.game != *game_account.key {
        msg!("Parent game account mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Load parent game for the fee-split bps ------------------------------------------------
    if game_account.owner != &PROGRAM_ID {
        msg!("Parent game account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let game_data = GameAccount::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if !game_data.is_initialized {
        msg!("Parent game account not initialized");
        return Err(ProgramError::UninitializedAccount);
    }

    // -- Validate the game protocol PDA ------------------------------------------------
    let (gp_pda, _gp_bump) = find_game_protocol_pda(&game_data.game_title);
    if gp_pda != *game_protocol.key {
        msg!("Game protocol PDA mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Validate the user account ------------------------------------------------
    if user_account.owner != &PROGRAM_ID {
        msg!("User account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let mut user_data = UserAccount::try_from_slice(&user_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if !user_data.is_initialized {
        msg!("User account not initialized");
        return Err(ProgramError::UninitializedAccount);
    }
    // The user account must belong to the signing player.
    if user_data.owner != *player.key {
        msg!("User account owner mismatch");
        return Err(ProgramError::IllegalOwner);
    }
    // The user account must be for this parent game.
    if user_data.game != *game_account.key {
        msg!("User account game mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    if user_data.current_ranked_game == *ranked_game_account.key {
        msg!("Player is already in the ranked game");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Resolve payment token ------------------------------------------------
    let token = if payment_token == PaymentToken::SOL.mint() {
        PaymentToken::SOL
    } else if payment_token == SKR_MINT {
        PaymentToken::SKR
    } else if payment_token == SLICE_MINT {
        PaymentToken::SLICE
    } else if payment_token == USDC_MINT {
        PaymentToken::USDC
    } else {
        msg!("Unsupported payment token");
        return Err(ProgramError::InvalidAccountData);
    };

    // The processor expects dev + protocol shares summing to 10000.
    // game_to_protocol_share_bps is the protocol's slice; dev keeps the rest.
    let protocol_share = game_data.game_to_protocol_share_bps;
    let dev_share = (BPS_DENOMINATOR as u16)
        .checked_sub(protocol_share)
        .ok_or(ProgramError::InvalidInstructionData)?;

    let spl_accounts = match token {
        PaymentToken::SOL => None,
        _ => Some(RankedSplPaymentAccounts {
            payer_ata,
            ranked_game_ata: ranked_escrow_ata,
            game_ata,
            protocol_ata,
            mint,
        }),
    };

    process_ranked_entry_payment(
        player,
        ranked_game_account,
        game_account,
        game_protocol,
        oracle_account,
        token_program,
        system_program,
        spl_accounts,
        ranked_game_data.entry_fee,
        dev_share,
        protocol_share,
        token,
    )?;

    // -- Update + persist ranked game state ------------------------------------------------
    ranked_game_data.participant_count = ranked_game_data
        .participant_count
        .checked_add(1)
        .ok_or(ProgramError::InvalidAccountData)?;
    ranked_game_data.serialize(
        &mut &mut ranked_game_account.data.borrow_mut()[..],
    )?;

    // -- Update + persist user account ------------------------------------------------
    user_data.current_ranked_game = *ranked_game_account.key;
    user_data.total_ranked_games_played = user_data
        .total_ranked_games_played
        .checked_add(1)
        .ok_or(ProgramError::InvalidAccountData)?;
    user_data.serialize(&mut &mut user_account.data.borrow_mut()[..])?;

    msg!("Player {} joined ranked game {}", player.key, ranked_game_id);
    Ok(())
}

pub fn update_ranked_game_status<'a>(
    accounts: &'a [AccountInfo<'a>],
    ranked_game_id: u64,
    new_status: u8,
) -> ProgramResult {
    let iter = &mut accounts.iter();

    let authority = next_account_info(iter)?; // signer
    let ranked_game_account = next_account_info(iter)?; // PDA (state + SOL escrow)

    if !authority.is_signer {
        msg!("Authority must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    if *authority.key != AUTHORITY {
        msg!("Only the protocol authority can update ranked game status");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Validate ranked game state ------------------------------------------------
    if ranked_game_account.owner != &PROGRAM_ID {
        msg!("Ranked game account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let (pda, _bump) = find_ranked_game_pda(ranked_game_id);
    if pda != *ranked_game_account.key {
        msg!("Ranked game account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let mut ranked_game_data = RankedGameState::try_from_slice(
        &ranked_game_account.data.borrow(),
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;

    if ranked_game_data.discriminator != RANKED_GAME_DISCRIMINATOR {
        msg!("Invalid ranked game discriminator");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Update status ------------------------------------------------
    ranked_game_data.status = match new_status {
        0 => RankedGameStatus::Open,
        1 => RankedGameStatus::InProgress,
        2 => RankedGameStatus::Finished,
        _ => {
            msg!("Invalid ranked game status value");
            return Err(ProgramError::InvalidInstructionData);
        }
    };

    // -- Persist updated state ------------------------------------------------
    ranked_game_data.serialize(
        &mut &mut ranked_game_account.data.borrow_mut()[..],
    )?;

    msg!(
        "Ranked game {} status updated to {:?}",
        ranked_game_id,
        ranked_game_data.status
    );
    Ok(())
}

/// Pay `payout_bps` of the PDA's spendable lamport balance to the winner.
/// Returns the USD-micro value of what was paid.
fn pay_sol_pool<'a>(
    ranked_game_account: &AccountInfo<'a>,
    winner: &AccountInfo<'a>,
    oracle: &crate::state::oracle::OraclePriceState,
    payout_bps: u16,
) -> Result<u64, ProgramError> {
    let rent = Rent::get()?;
    let min_rent = rent.minimum_balance(RankedGameState::LEN);
    let available_lamports = ranked_game_account.lamports().saturating_sub(min_rent);
    if available_lamports == 0 {
        return Ok(0);
    }

    let lamports_to_send = ((available_lamports as u128)
        .saturating_mul(payout_bps as u128)
        / BPS_DENOMINATOR as u128) as u64;
    if lamports_to_send == 0 {
        return Ok(0);
    }

    **ranked_game_account.try_borrow_mut_lamports()? -= lamports_to_send;
    **winner.try_borrow_mut_lamports()? += lamports_to_send;

    // price_for(SOL) is micro-USD per lamport, so usd_micro = lamports * price.
    let price_micro_usd = price_for(oracle, &PaymentToken::SOL);
    let paid_usd_micro = (lamports_to_send as u128)
        .checked_mul(price_micro_usd as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let paid_usd_micro = paid_usd_micro.min(u64::MAX as u128) as u64;

    msg!("SOL pool: paid {} lamports (~{} USD micro)", lamports_to_send, paid_usd_micro);
    Ok(paid_usd_micro)
}

/// Pay `payout_bps` of one SPL escrow pool's balance to the winner.
/// Creates the winner's ATA if missing. Returns the USD-micro value paid.
#[allow(clippy::too_many_arguments)]
fn pay_spl_pool<'a>(
    authority: &AccountInfo<'a>,
    ranked_game_account: &AccountInfo<'a>,
    winner: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    escrow_ata: &AccountInfo<'a>,
    winner_ata: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    ata_program: &AccountInfo<'a>,
    rent_sysvar: &AccountInfo<'a>,
    oracle: &crate::state::oracle::OraclePriceState,
    token: PaymentToken,
    ranked_game_id: u64,
    bump: u8,
    payout_bps: u16,
    label: &str,
) -> Result<u64, ProgramError> {
    let expected_escrow = spl_associated_token_account::get_associated_token_address(
        ranked_game_account.key,
        mint.key,
    );
    if expected_escrow != *escrow_ata.key {
        msg!("{} escrow ATA address mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }

    // Read escrow token balance (SPL Token amount is u64 LE at offset 64).
    let escrow_balance = {
        let data = escrow_ata.data.borrow();
        if data.len() < 72 {
            return Ok(0);
        }
        u64::from_le_bytes(
            data[64..72].try_into().map_err(|_| ProgramError::InvalidAccountData)?,
        )
    };
    if escrow_balance == 0 {
        return Ok(0);
    }

    let units_to_send = ((escrow_balance as u128)
        .saturating_mul(payout_bps as u128)
        / BPS_DENOMINATOR as u128) as u64;
    if units_to_send == 0 {
        return Ok(0);
    }

    let expected_winner_ata = spl_associated_token_account::get_associated_token_address(
        winner.key,
        mint.key,
    );
    if expected_winner_ata != *winner_ata.key {
        msg!("{} winner ATA address mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }
    if winner_ata.data_is_empty() {
        invoke(
            &create_associated_token_account(
                authority.key,
                winner.key,
                mint.key,
                token_program.key,
            ),
            &[
                authority.clone(),
                winner_ata.clone(),
                winner.clone(),
                mint.clone(),
                system_program.clone(),
                token_program.clone(),
                ata_program.clone(),
                rent_sysvar.clone(),
            ],
        )?;
        msg!("{} winner ATA created", label);
    }

    let decimals = token_decimals(&token);

    invoke_signed(
        &transfer_checked(
            token_program.key,
            escrow_ata.key,
            mint.key,
            winner_ata.key,
            ranked_game_account.key,
            &[],
            units_to_send,
            decimals,
        )?,
        &[
            escrow_ata.clone(),
            mint.clone(),
            winner_ata.clone(),
            ranked_game_account.clone(),
            token_program.clone(),
        ],
        &[&[b"igp_ranked_game", &ranked_game_id.to_le_bytes(), &[bump]]],
    )?;

    // usd_micro = units * price_micro_usd_per_token / 10^decimals
    let price_micro_usd = price_for(oracle, &token);
    let paid_usd_micro = (units_to_send as u128)
        .checked_mul(price_micro_usd as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?
        .checked_div(10_u128.pow(decimals as u32))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let paid_usd_micro = paid_usd_micro.min(u64::MAX as u128) as u64;

    msg!("{} pool: paid {} base units (~{} USD micro)", label, units_to_send, paid_usd_micro);
    Ok(paid_usd_micro)
}

pub fn reward_winner<'a>(
    accounts: &'a [AccountInfo<'a>],
    ranked_game_id: u64,
    payout_bps: u16, // 0..=10000 — percent of each pool to pay the winner
) -> ProgramResult {
    let iter = &mut accounts.iter();

    let authority = next_account_info(iter)?;           // signer (protocol authority)
    let ranked_game_account = next_account_info(iter)?; // PDA (state + SOL escrow owner)
    let winner = next_account_info(iter)?;              // winner wallet (ATA owner + SOL recipient)
    let winner_user_account = next_account_info(iter)?; // winner's per-game UserAccount PDA
    let oracle_account = next_account_info(iter)?;      // oracle PDA (for USD reward accounting)
    let system_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let ata_program = next_account_info(iter)?;         // SPL ATA program
    let rent_sysvar = next_account_info(iter)?;         // rent sysvar (for ATA CPI)

    // Per-token accounts: (mint, escrow ATA owned by ranked game PDA, winner ATA).
    // Order MUST be USDC, SKR, SLICE to match the client.
    let usdc_mint = next_account_info(iter)?;
    let usdc_escrow_ata = next_account_info(iter)?;
    let usdc_winner_ata = next_account_info(iter)?;
    let skr_mint = next_account_info(iter)?;
    let skr_escrow_ata = next_account_info(iter)?;
    let skr_winner_ata = next_account_info(iter)?;
    let slice_mint = next_account_info(iter)?;
    let slice_escrow_ata = next_account_info(iter)?;
    let slice_winner_ata = next_account_info(iter)?;

    if !authority.is_signer {
        msg!("Authority must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *authority.key != AUTHORITY {
        msg!("Only the protocol authority can reward winners");
        return Err(ProgramError::InvalidAccountData);
    }
    if payout_bps as u64 > BPS_DENOMINATOR {
        msg!("payout_bps exceeds 10000 (100%)");
        return Err(ProgramError::InvalidInstructionData);
    }

    // -- Validate mints -----------------------------------------------------------
    if *usdc_mint.key != USDC_MINT {
        msg!("USDC mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    if *skr_mint.key != SKR_MINT {
        msg!("SKR mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    if *slice_mint.key != SLICE_MINT {
        msg!("SLICE mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Validate ranked game state -----------------------------------------------
    if ranked_game_account.owner != &PROGRAM_ID {
        msg!("Ranked game account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let (pda, bump) = find_ranked_game_pda(ranked_game_id);
    if pda != *ranked_game_account.key {
        msg!("Ranked game account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let ranked_game_data = RankedGameState::try_from_slice(
        &ranked_game_account.data.borrow(),
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;

    if ranked_game_data.discriminator != RANKED_GAME_DISCRIMINATOR {
        msg!("Invalid ranked game discriminator");
        return Err(ProgramError::InvalidAccountData);
    }
    if ranked_game_data.status != RankedGameStatus::Finished {
        msg!("Ranked game must be finished before rewarding winners");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Validate the winner's user account ---------------------------------------
    if winner_user_account.owner != &PROGRAM_ID {
        msg!("Winner user account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let mut winner_user_data = UserAccount::try_from_slice(&winner_user_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if !winner_user_data.is_initialized {
        msg!("Winner user account not initialized");
        return Err(ProgramError::UninitializedAccount);
    }
    if winner_user_data.owner != *winner.key {
        msg!("Winner user account owner mismatch");
        return Err(ProgramError::IllegalOwner);
    }
    if winner_user_data.current_ranked_game != *ranked_game_account.key {
        msg!("Winner is not a participant of this ranked game (or already rewarded)");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Load oracle once (for USD reward accounting) -----------------------------
    let oracle = load_oracle(oracle_account)?;

    // Accumulate the USD-micro value of every pool paid out.
    let mut total_paid_usd_micro: u64 = 0;

    // ---- Pool 1: SOL (native lamports held by the ranked game PDA) --------------
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(
        pay_sol_pool(ranked_game_account, winner, &oracle, payout_bps)?,
    );

    // ---- Pools 2-4: SPL tokens --------------------------------------------------
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(pay_spl_pool(
        authority, ranked_game_account, winner,
        usdc_mint, usdc_escrow_ata, usdc_winner_ata,
        system_program, token_program, ata_program, rent_sysvar,
        &oracle, PaymentToken::USDC, ranked_game_id, bump, payout_bps, "USDC",
    )?);
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(pay_spl_pool(
        authority, ranked_game_account, winner,
        skr_mint, skr_escrow_ata, skr_winner_ata,
        system_program, token_program, ata_program, rent_sysvar,
        &oracle, PaymentToken::SKR, ranked_game_id, bump, payout_bps, "SKR",
    )?);
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(pay_spl_pool(
        authority, ranked_game_account, winner,
        slice_mint, slice_escrow_ata, slice_winner_ata,
        system_program, token_program, ata_program, rent_sysvar,
        &oracle, PaymentToken::SLICE, ranked_game_id, bump, payout_bps, "SLICE",
    )?);

    // -- Update winner accounting + clear current ranked game ---------------------
    winner_user_data.total_usd_rewards_micro = winner_user_data
        .total_usd_rewards_micro
        .checked_add(total_paid_usd_micro)
        .ok_or(ProgramError::InvalidAccountData)?;
    winner_user_data.total_wins = winner_user_data
        .total_wins
        .checked_add(1)
        .ok_or(ProgramError::InvalidAccountData)?;
    winner_user_data.current_ranked_game = Pubkey::default();
    winner_user_data.serialize(&mut &mut winner_user_account.data.borrow_mut()[..])?;

    msg!(
        "Rewarded winner {} | {} bps of each pool | ~{} USD micro | ranked game {}",
        winner.key,
        payout_bps,
        total_paid_usd_micro,
        ranked_game_id
    );

    Ok(())
}

pub fn close_ranked_game<'a>(
    accounts: &'a [AccountInfo<'a>],
    ranked_game_id: u64,
) -> ProgramResult {
    let iter = &mut accounts.iter();

    let authority = next_account_info(iter)?;           // signer (protocol authority)
    let ranked_game_account = next_account_info(iter)?; // PDA (state + SOL escrow owner)
    let token_program = next_account_info(iter)?;

    // Per-token: (escrow ATA owned by ranked game PDA, authority ATA to sweep dust into).
    // Order MUST be USDC, SKR, SLICE to match the client.
    let usdc_mint = next_account_info(iter)?;
    let usdc_escrow_ata = next_account_info(iter)?;
    let usdc_authority_ata = next_account_info(iter)?;
    let skr_mint = next_account_info(iter)?;
    let skr_escrow_ata = next_account_info(iter)?;
    let skr_authority_ata = next_account_info(iter)?;
    let slice_mint = next_account_info(iter)?;
    let slice_escrow_ata = next_account_info(iter)?;
    let slice_authority_ata = next_account_info(iter)?;

    if !authority.is_signer {
        msg!("Authority must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *authority.key != AUTHORITY {
        msg!("Only the protocol authority can close ranked games");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Validate mints -----------------------------------------------------------
    if *usdc_mint.key != USDC_MINT {
        msg!("USDC mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    if *skr_mint.key != SKR_MINT {
        msg!("SKR mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    if *slice_mint.key != SLICE_MINT {
        msg!("SLICE mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Validate ranked game state -----------------------------------------------
    if ranked_game_account.owner != &PROGRAM_ID {
        msg!("Ranked game account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let (pda, bump) = find_ranked_game_pda(ranked_game_id);
    if pda != *ranked_game_account.key {
        msg!("Ranked game account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let signer_seeds: &[&[u8]] = &[b"igp_ranked_game", &ranked_game_id.to_le_bytes(), &[bump]];

    // -- Close the 3 escrow ATAs (sweep any dust, then close) ---------------------
    close_escrow_ata(
        ranked_game_account, authority, token_program,
        usdc_mint, usdc_escrow_ata, usdc_authority_ata,
        signer_seeds, "USDC",
    )?;
    close_escrow_ata(
        ranked_game_account, authority, token_program,
        skr_mint, skr_escrow_ata, skr_authority_ata,
        signer_seeds, "SKR",
    )?;
    close_escrow_ata(
        ranked_game_account, authority, token_program,
        slice_mint, slice_escrow_ata, slice_authority_ata,
        signer_seeds, "SLICE",
    )?;

    // -- Close the state / SOL escrow account -------------------------------------
    // Lamports return to the authority. Do this AFTER the CPIs above, which need
    // the PDA to still hold its data + lamports to sign.
    ranked_game_account.data.borrow_mut().fill(0);
    **authority.try_borrow_mut_lamports()? += ranked_game_account.lamports();
    **ranked_game_account.try_borrow_mut_lamports()? = 0;

    msg!("Ranked game {} closed; 3 escrow ATAs closed and all rent returned to authority", ranked_game_id);
    Ok(())
}

/// Sweep any remaining balance from one escrow ATA to the authority's ATA, then
/// close the escrow ATA (rent lamports go to the authority). No-op if the escrow
/// ATA was never created.
#[allow(clippy::too_many_arguments)]
fn close_escrow_ata<'a>(
    ranked_game_pda: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    escrow_ata: &AccountInfo<'a>,
    authority_ata: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
    label: &str,
) -> ProgramResult {
    // Nothing to do if the ATA doesn't exist.
    if escrow_ata.data_is_empty() {
        msg!("{} escrow ATA does not exist, skipping", label);
        return Ok(());
    }

    // Verify the escrow ATA is the canonical (ranked_game_pda, mint) address.
    let expected_escrow = spl_associated_token_account::get_associated_token_address(
        ranked_game_pda.key,
        mint.key,
    );
    if expected_escrow != *escrow_ata.key {
        msg!("{} escrow ATA address mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }

    // Read the current escrow balance (SPL Token amount is u64 LE at offset 64).
    let escrow_balance = {
        let data = escrow_ata.data.borrow();
        if data.len() < 72 {
            return Err(ProgramError::InvalidAccountData);
        }
        u64::from_le_bytes(
            data[64..72].try_into().map_err(|_| ProgramError::InvalidAccountData)?,
        )
    };

    // Sweep any remaining tokens (dust) to the authority's ATA before closing,
    // since SPL close_account requires a zero balance.
    if escrow_balance > 0 {
        // Verify destination is the authority's canonical ATA for this mint.
        let expected_authority_ata = spl_associated_token_account::get_associated_token_address(
            authority.key,
            mint.key,
        );
        if expected_authority_ata != *authority_ata.key {
            msg!("{} authority ATA address mismatch", label);
            return Err(ProgramError::InvalidAccountData);
        }
        if authority_ata.data_is_empty() {
            msg!("{} authority ATA must exist to receive dust", label);
            return Err(ProgramError::UninitializedAccount);
        }

        // decimals from the mint (offset 44) for transfer_checked.
        let decimals = {
            let data = mint.data.borrow();
            if data.len() < 45 {
                return Err(ProgramError::InvalidAccountData);
            }
            data[44]
        };

        invoke_signed(
            &transfer_checked(
                token_program.key,
                escrow_ata.key,
                mint.key,
                authority_ata.key,
                ranked_game_pda.key,
                &[],
                escrow_balance,
                decimals,
            )?,
            &[
                escrow_ata.clone(),
                mint.clone(),
                authority_ata.clone(),
                ranked_game_pda.clone(),
                token_program.clone(),
            ],
            &[signer_seeds],
        )?;
        msg!("{} swept {} dust units to authority", label, escrow_balance);
    }

    // Close the now-empty escrow ATA; rent lamports go to the authority.
    invoke_signed(
        &spl_token::instruction::close_account(
            token_program.key,
            escrow_ata.key,
            authority.key,        // rent destination
            ranked_game_pda.key,  // account owner (authority over the token account)
            &[],
        )?,
        &[
            escrow_ata.clone(),
            authority.clone(),
            ranked_game_pda.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )?;

    msg!("{} escrow ATA closed, rent returned to authority", label);
    Ok(())
}