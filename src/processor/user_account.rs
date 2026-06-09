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
    program_pack::Pack,
};

use spl_token::{
    instruction::transfer_checked,
    instruction::transfer,
    state::Account as SplTokenAccount,
};
use spl_associated_token_account::get_associated_token_address;

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
        created_at: timestamp,
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

pub fn withdraw_rewards<'a>(
    accounts: &'a [AccountInfo<'a>],
    timestamp: u64,
) -> ProgramResult {
    let iter = &mut accounts.iter();

    let user_wallet = next_account_info(iter)?;     // owner of the user account & signer
    let game_account = next_account_info(iter)?;    // game PDA (for game_title -> user PDA derivation)
    let user_account = next_account_info(iter)?;    // user's UserAccount PDA (owns the source ATAs + holds SOL)
    let user_skr_ata = next_account_info(iter)?;
    let user_slice_ata = next_account_info(iter)?;
    let user_usdc_ata = next_account_info(iter)?;
    let destination_skr_ata = next_account_info(iter)?;
    let destination_slice_ata = next_account_info(iter)?;
    let destination_usdc_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !user_wallet.is_signer {
        msg!("User wallet must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // -- Validate the user account ------------------------------------------------
    if user_account.owner != &PROGRAM_ID {
        msg!("User account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let user_data = UserAccount::try_from_slice(&user_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if !user_data.is_initialized {
        msg!("User account not initialized");
        return Err(ProgramError::UninitializedAccount);
    }

    // Only the wallet that owns this user account may withdraw its rewards.
    if user_data.owner != *user_wallet.key {
        msg!("Only the user account owner can withdraw rewards");
        return Err(ProgramError::IllegalOwner);
    }

    // The user account must belong to the supplied game account.
    if user_data.game != *game_account.key {
        msg!("User account game mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Re-derive the user PDA + bump so it can sign the SPL transfers -----------
    let (user_pda, user_bump) =
        find_user_account_pda(&user_data.owner, &user_data.game, timestamp);
    if user_pda != *user_account.key {
        msg!("User account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let ts_bytes = timestamp.to_le_bytes();
    let signer_seeds: &[&[u8]] = &[
        b"igp_user",
        user_data.owner.as_ref(),
        user_data.game.as_ref(),
        &ts_bytes,
        &[user_bump],
    ];

    // -- Drain each SPL pool + sweep surplus SOL to the user wallet ---------------
    withdraw_user_rewards(
        user_account, user_wallet, user_skr_ata, destination_skr_ata,
        &PaymentToken::SKR, token_program, signer_seeds, false, "SKR",
    )?;
    withdraw_user_rewards(
        user_account, user_wallet, user_slice_ata, destination_slice_ata,
        &PaymentToken::SLICE, token_program, signer_seeds, false, "SLICE",
    )?;
    // Sweep SOL on the final call so all three SPL transfers settle first.
    withdraw_user_rewards(
        user_account, user_wallet, user_usdc_ata, destination_usdc_ata,
        &PaymentToken::USDC, token_program, signer_seeds, true, "USDC",
    )?;

    msg!(
        "Reward withdraw complete for user account {}",
        user_account.key
    );
    Ok(())
}

/// Transfer the entire balance of one user-owned ATA to a destination ATA, and
/// (optionally) sweep the user PDA's surplus native SOL (above rent-exemption)
/// to the user wallet. No-op on an empty/uninitialized source ATA.
#[allow(clippy::too_many_arguments)]
fn withdraw_user_rewards<'a>(
    user_account: &AccountInfo<'a>,
    user_wallet: &AccountInfo<'a>,
    source_ata: &AccountInfo<'a>,
    destination_ata: &AccountInfo<'a>,
    token: &PaymentToken,
    token_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
    sweep_sol: bool,
    label: &str,
) -> ProgramResult {
    let mint = token.mint();

    // Verify the source is the user PDA's canonical ATA for this mint.
    let expected_source = get_associated_token_address(user_account.key, &mint);
    if expected_source != *source_ata.key {
        msg!("{} source ATA address mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }

    // Skip the SPL transfer if the source doesn't exist yet.
    if source_ata.data_is_empty() {
        msg!("{} source ATA does not exist, skipping", label);
    } else {
        let source_state = SplTokenAccount::unpack(&source_ata.data.borrow())
            .map_err(|_| ProgramError::InvalidAccountData)?;

        if source_state.owner != *user_account.key {
            msg!("{} source ATA not owned by user PDA", label);
            return Err(ProgramError::InvalidAccountData);
        }
        if source_state.mint != mint {
            msg!("{} source ATA mint mismatch", label);
            return Err(ProgramError::InvalidAccountData);
        }

        let amount = source_state.amount;
        if amount == 0 {
            msg!("{} pool empty, nothing to withdraw", label);
        } else {
            invoke_signed(
                &transfer(
                    token_program.key,
                    source_ata.key,
                    destination_ata.key,
                    user_account.key, // authority = user PDA (owns the source ATA)
                    &[],
                    amount,
                )?,
                &[
                    source_ata.clone(),
                    destination_ata.clone(),
                    user_account.clone(),
                    token_program.clone(),
                ],
                &[signer_seeds],
            )?;
            msg!("{} withdrew {} base units to destination", label, amount);
        }
    }

    // Sweep surplus native SOL (above rent-exemption) from the user PDA.
    if sweep_sol {
        let rent = Rent::get()?;
        let min_rent = rent.minimum_balance(user_account.data_len());
        let withdrawable = user_account.lamports().saturating_sub(min_rent);

        if withdrawable > 0 {
            // PDA is program-owned: move lamports by direct field mutation.
            **user_account.try_borrow_mut_lamports()? -= withdrawable;
            **user_wallet.try_borrow_mut_lamports()? += withdrawable;
            msg!("Withdrew {} excess SOL lamports to user wallet", withdrawable);
        } else {
            msg!("No excess SOL to withdraw");
        }
    }

    Ok(())
}