use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke,
    program::invoke_signed,
    program_error::ProgramError,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
    pubkey,
    pubkey::Pubkey,
};

use crate::{
    constants::{PROGRAM_ID, PaymentToken, SKR_MINT, SLICE_MINT, USDC_MINT},
    instruction::IndieGamesInstruction,
    helpers::{validate_dev_fee_bps, find_game_pda, find_user_account_pda},
    state::{
        game::{GameAccount, GAME_TITLE_MAX_LEN},
        user_account::{UserAccount, USERNAME_MAX_LEN},
    },
    processor::payment::{process_payment, SplPaymentAccounts},
};


// ----------------------------------------------------------
// CREATE USER ACCOUNT
// ----------------------------------------------------------
pub fn create_user_account<'a>(
    accounts: &'a [AccountInfo<'a>],
    version: u8,
    username: [u8; USERNAME_MAX_LEN],
    timestamp: u64,
    payment_token: Pubkey,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?; // owner & signer
    let user_account = next_account_info(iter)?; // PDA
    let game_account = next_account_info(iter)?; // The game PDA this user account is associated with
    let system_program = next_account_info(iter)?;

    if !owner.is_signer {
        msg!("Owner must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    if username.len() > USERNAME_MAX_LEN {
        msg!("Username too long");
        return Err(ProgramError::InvalidInstructionData);
    }

    if !user_account.data_is_empty() {
        msg!("User account already initialized");
        return Err(ProgramError::AccountAlreadyInitialized);
    }

    // Validate that the provided game account is a valid game PDA
    let mut game_data = {
        let mut slice: &[u8] = &game_account.data.borrow();
        GameAccount::deserialize(&mut slice)?
    };
    if !game_data.is_initialized {
        msg!("Associated game account is not initialized");
        return Err(ProgramError::InvalidAccountData);
    }

    // First, check if the game cost is greater than 0.
    // If it is, make a transfer from the user wallet to the game wallet for the specified amount.
    // We need to also use the oracle data here because the price is set in usdc,
    // but the user can pay in SOL, SKR, SLICE or USDC right now.
    if game_data.game_cost > 0 {
        let payment_token_enum = if payment_token == USDC_MINT {
            PaymentToken::USDC
        } else if payment_token == SKR_MINT {
            PaymentToken::SKR
        } else if payment_token == SLICE_MINT {
            PaymentToken::SLICE
        } else if payment_token == pubkey!("So11111111111111111111111111111111111111112") {
            PaymentToken::SOL
        } else {
            msg!("Unsupported payment token");
            return Err(ProgramError::InvalidInstructionData);
        };

        // payment accounts, appended after system_program
        let game_protocol  = next_account_info(iter)?;
        let oracle_account = next_account_info(iter)?;
        let token_program  = next_account_info(iter)?;

        let spl_accounts = if payment_token_enum != PaymentToken::SOL {
            Some(SplPaymentAccounts {
                payer_ata:    next_account_info(iter)?,
                game_ata:     next_account_info(iter)?,
                protocol_ata: next_account_info(iter)?,
                mint:         next_account_info(iter)?,
            })
        } else {
            None
        };

        process_payment(
            owner,
            game_account,
            game_protocol,
            oracle_account,
            token_program,
            system_program,
            spl_accounts,
            game_data.game_cost,
            payment_token_enum,
        )?;
    }


    let (pda, bump) = find_user_account_pda(owner.key, game_account.key, timestamp);

    if pda != *user_account.key {
        msg!("User account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let rent = Rent::get()?;
    let required_lamports = rent.minimum_balance(UserAccount::MAX_SIZE);

    invoke_signed(
        &system_instruction::create_account(
            owner.key,
            user_account.key,
            required_lamports,
            UserAccount::MAX_SIZE as u64,
            &PROGRAM_ID,
        ),
        &[owner.clone(), user_account.clone(), system_program.clone()],
        &[&[b"igp_user", owner.key.as_ref(), game_account.key.as_ref(), &timestamp.to_le_bytes(), &[bump]]],
    )?;

    let mut user_data = UserAccount {
        is_initialized: true,
        version,
        bump,
        owner: *owner.key,
        game: *game_account.key,
        username,
        current_ranked_game: Pubkey::default(),
        total_ranked_games_played: 0,
        total_wins: 0,
        total_usd_spent_micro: 0,
        total_usd_rewards_micro: 0,
    };

    user_data.serialize(&mut &mut user_account.data.borrow_mut()[..])?;

    let username_str = std::string::String::from_utf8_lossy(&username);
    let username_str = username_str.trim_matches(char::from(0));
    msg!("User account created successfully for owner {} with username {:?}", owner.key, username_str);

    // Increment total_users in the associated game account
    game_data.total_users = game_data.total_users.checked_add(1).ok_or(ProgramError::ArithmeticOverflow)?;
    game_data.serialize(&mut &mut game_account.data.borrow_mut()[..])?;

    let title = String::from_utf8_lossy(&game_data.game_title);
    let title = title.trim_matches(char::from(0));
    msg!("[{}] Total users so far: {}", title, game_data.total_users);

    Ok(())
}