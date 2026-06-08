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