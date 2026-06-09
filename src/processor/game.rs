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
    constants::{PROGRAM_ID, CURRENT_GAME_STATE_VERSION, PaymentToken},
    instruction::IndieGamesInstruction,
    helpers::{validate_dev_fee_bps, find_game_pda, find_user_account_pda, find_game_protocol_pda, assert_authority, token_decimals, load_oracle, BPS_DENOMINATOR},
    state::{
        game::{GameAccountV1, GameAccount, GAME_TITLE_MAX_LEN},
        user_account::UserAccount,
    },
    processor::oracle::{price_for},
};

// ----------------------------------------------------------
// CREATE GAME
// ----------------------------------------------------------

pub fn create_game(
    accounts: &[AccountInfo],
    version: u8,
    game_title: [u8; GAME_TITLE_MAX_LEN],
    dev_fee_bps: u16,
    game_to_protocol_share_bps: u16,
    game_cost: u64,
    game_url: [u8; 64],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let game_account = next_account_info(iter)?;
    let game_account_skr_ata = next_account_info(iter)?;
    let game_account_slice_ata = next_account_info(iter)?;
    let game_account_usdc_ata = next_account_info(iter)?;
    let game_protocol_account = next_account_info(iter)?;
    let game_protocol_account_skr_ata = next_account_info(iter)?;
    let game_protocol_account_slice_ata = next_account_info(iter)?;
    let game_protocol_account_usdc_ata = next_account_info(iter)?;
    let skr_mint = next_account_info(iter)?;
    let slice_mint = next_account_info(iter)?;
    let usdc_mint = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let associated_token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !owner.is_signer {
        msg!("Owner must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    if game_title.len() > GAME_TITLE_MAX_LEN {
        msg!("Game title too long");
        return Err(ProgramError::InvalidInstructionData);
    }

    // Create the game protocol PDA in case it doesn't exist yet
    // the game protocol PDA is an empty account that has greater than 0 lamports
    let (pda, bump) = find_game_protocol_pda(&game_title);
    if pda != *game_protocol_account.key {
        msg!("Invalid game protocol account PDA");
        return Err(ProgramError::InvalidAccountData);
    }

    if game_protocol_account.owner != &PROGRAM_ID {
        let rent = Rent::get()?;
        let space = 0usize;
        let lamports = rent.minimum_balance(space);

        // MUST match find_game_protocol_pda's seed exactly:
        let signer_seeds: &[&[u8]] = &[
            b"igp_game_protocol",
            &game_title,
            &[bump],
        ];

        let current = game_protocol_account.lamports();
        if current < lamports {
            invoke(
                &system_instruction::transfer(owner.key, game_protocol_account.key, lamports - current),
                &[owner.clone(), game_protocol_account.clone(), system_program.clone()],
            )?;
        }
        invoke_signed(
            &system_instruction::allocate(game_protocol_account.key, space as u64),
            &[game_protocol_account.clone(), system_program.clone()],
            &[signer_seeds],
        )?;
        invoke_signed(
            &system_instruction::assign(game_protocol_account.key, &PROGRAM_ID),
            &[game_protocol_account.clone(), system_program.clone()],
            &[signer_seeds],
        )?;

        // Pair each token with its passed-in ATA + mint AccountInfo
        let ata_specs = [
            (PaymentToken::SKR,   game_protocol_account_skr_ata,   skr_mint),
            (PaymentToken::SLICE, game_protocol_account_slice_ata, slice_mint),
            (PaymentToken::USDC,  game_protocol_account_usdc_ata,  usdc_mint),
        ];

        for (token, ata_info, mint_info) in ata_specs {
            // verify the passed mint matches the expected mint for this token
            if mint_info.key != &token.mint() {
                msg!("Wrong mint for {:?}", token);
                return Err(ProgramError::InvalidAccountData);
            }
            // verify the passed ATA is the canonical address
            let expected_ata = get_associated_token_address(
                game_protocol_account.key,
                mint_info.key,
            );
            if ata_info.key != &expected_ata {
                msg!("Wrong ATA for {:?}", token);
                return Err(ProgramError::InvalidAccountData);
            }

            // idempotent create avoids failing if front-run / already created
            let ix = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                owner.key,                  // funder (signs at tx level)
                game_protocol_account.key,  // wallet
                mint_info.key,
                &spl_token::ID,
            );
            // plain invoke - funder signs normally; PDA wallet does NOT need to sign for ATA creation
            invoke(
                &ix,
                &[
                    owner.clone(),
                    ata_info.clone(),
                    game_protocol_account.clone(),
                    mint_info.clone(),
                    system_program.clone(),
                    token_program.clone(),
                    associated_token_program.clone(),
                ],
            )?;
            msg!("Created ATA for {:?} at {}", token, expected_ata);
        }
    }

    validate_dev_fee_bps(dev_fee_bps)?;

    let (game_pda, game_bump) = find_game_pda(&game_title);
    if game_pda != *game_account.key {
        msg!("Invalid game account PDA");
        return Err(ProgramError::InvalidAccountData);
    }

    // Create + initialize the game account only if it doesn't exist yet.
    // If it already exists, fall through and (re)ensure its ATAs below.
    if game_account.data_is_empty() {
        let rent = Rent::get()?;
        let space = GameAccount::MAX_SIZE;
        let needed = rent.minimum_balance(space);

        let game_seeds: &[&[u8]] = &[b"igp_game", &game_title, &[game_bump]];

        let current = game_account.lamports();
        if current < needed {
            invoke(
                &system_instruction::transfer(owner.key, game_account.key, needed - current),
                &[owner.clone(), game_account.clone(), system_program.clone()],
            )?;
        }
        invoke_signed(
            &system_instruction::allocate(game_account.key, space as u64),
            &[game_account.clone(), system_program.clone()],
            &[game_seeds],
        )?;
        invoke_signed(
            &system_instruction::assign(game_account.key, &PROGRAM_ID),
            &[game_account.clone(), system_program.clone()],
            &[game_seeds],
        )?;

        GameAccount {
            is_initialized: true,
            version,
            game_title,
            owner: *owner.key,
            dev_fee_bps,
            game_to_protocol_share_bps,
            game_cost,
            game_url,
            total_users: 0,
            total_revenue_usd_micro: 0,
        }
        .serialize(&mut &mut game_account.data.borrow_mut()[..])?;

        msg!(
            "Game '{}' created with dev fee {} bps and protocol share {} bps",
            String::from_utf8_lossy(&game_title),
            dev_fee_bps,
            game_to_protocol_share_bps
        );
    } else {
        // Already initialized - verify it's actually ours and owned by us before
        // proceeding to ensure ATAs, so this re-call path can't be abused.
        if game_account.owner != &PROGRAM_ID {
            msg!("Game account exists but is not owned by this program");
            return Err(ProgramError::IllegalOwner);
        }

        let existing = GameAccount::try_from_slice(&game_account.data.borrow())?;
        if !existing.is_initialized {
            msg!("Game account present but not initialized");
            return Err(ProgramError::UninitializedAccount);
        }
        /*if existing.owner != *owner.key {
            msg!("Signer is not the owner of this game");
            return Err(ProgramError::IllegalOwner);
        }*/

        msg!("Game '{}' already exists - ensuring ATAs", String::from_utf8_lossy(&game_title));
    }

    // Always (re)ensure the game account's ATAs - idempotent, safe on re-call.
    let game_ata_specs = [
        (PaymentToken::SKR,   game_account_skr_ata,   skr_mint),
        (PaymentToken::SLICE, game_account_slice_ata, slice_mint),
        (PaymentToken::USDC,  game_account_usdc_ata,  usdc_mint),
    ];

    for (token, ata_info, mint_info) in game_ata_specs {
        if mint_info.key != &token.mint() {
            msg!("Wrong mint for {:?}", token);
            return Err(ProgramError::InvalidAccountData);
        }
        let expected_ata = get_associated_token_address(
            game_account.key,
            mint_info.key,
        );
        if ata_info.key != &expected_ata {
            msg!("Wrong ATA for {:?}", token);
            return Err(ProgramError::InvalidAccountData);
        }
        let ix = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            owner.key,
            game_account.key,
            mint_info.key,
            &spl_token::ID,
        );
        invoke(
            &ix,
            &[
                owner.clone(),
                ata_info.clone(),
                game_account.clone(),
                mint_info.clone(),
                system_program.clone(),
                token_program.clone(),
                associated_token_program.clone(),
            ],
        )?;
        msg!("Ensured game ATA for {:?} at {}", token, expected_ata);
    }

    Ok(())
}

pub fn update_fee_share(
    accounts: &[AccountInfo],
    new_dev_fee_bps: u16,
    new_game_to_protocol_share_bps: u16,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?; // signer
    let game_account = next_account_info(iter)?; // PDA

    if !owner.is_signer {
        msg!("Owner must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    let mut game_data = GameAccount::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;

    if game_data.owner != *owner.key {
        msg!("Only the game owner can update fee shares");
        return Err(ProgramError::IllegalOwner);
    }

    validate_dev_fee_bps(new_dev_fee_bps)?;

    game_data.dev_fee_bps = new_dev_fee_bps;
    game_data.game_to_protocol_share_bps = new_game_to_protocol_share_bps;
    game_data.serialize(&mut &mut game_account.data.borrow_mut()[..])?;

    msg!(
        "Updated fee shares: dev fee {} bps, protocol share {} bps",
        new_dev_fee_bps,
        new_game_to_protocol_share_bps
    );
    Ok(())
}

pub fn update_game_cost(
    accounts: &[AccountInfo],
    new_game_cost: u64,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?; // signer
    let game_account = next_account_info(iter)?; // PDA

    if !owner.is_signer {
        msg!("Owner must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    let mut game_data = GameAccount::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;

    if game_data.owner != *owner.key {
        msg!("Only the game owner can update the game cost");
        return Err(ProgramError::IllegalOwner);
    }

    game_data.game_cost = new_game_cost;
    game_data.serialize(&mut &mut game_account.data.borrow_mut()[..])?;

    msg!(
        "Updated game cost to {} micro_usd",
        new_game_cost,
    );
    Ok(())
}

pub fn transfer_game_ownership(
    accounts: &[AccountInfo],
    new_owner: Pubkey,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let current_owner = next_account_info(iter)?; // signer
    let game_account = next_account_info(iter)?; // PDA

    if !current_owner.is_signer {
        msg!("Current owner must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    let mut game_data = GameAccount::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;

    if game_data.owner != *current_owner.key {
        msg!("Only the current game owner can transfer ownership");
        return Err(ProgramError::IllegalOwner);
    }

    game_data.owner = new_owner;
    game_data.serialize(&mut &mut game_account.data.borrow_mut()[..])?;

    msg!(
        "Transferred game ownership to {}",
        new_owner
    );
    Ok(())
}

pub fn upgrade_game_version(
    accounts: &[AccountInfo],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let authority = next_account_info(iter)?; // signer
    let game_account = next_account_info(iter)?; // PDA
    let system_program = next_account_info(iter)?; // needed cause we have to grow the account

    assert_authority(authority)?;

    // Read version byte to check if upgrade is needed, without loading the entire account data yet
    let old_version = {
        let data = game_account.data.borrow();
        if data.len() < 2 {
            msg!("Game account data too small to contain version");
            return Err(ProgramError::InvalidAccountData);
        }
        data[1]
    };

    if old_version >= CURRENT_GAME_STATE_VERSION {
        msg!("Game state is already at version {} or higher, no upgrade needed", CURRENT_GAME_STATE_VERSION);
        return Ok(());
    }

    msg!("Upgrading game state from version {} to {}", old_version, CURRENT_GAME_STATE_VERSION);

    // Deserialize using the old layout (in the future there can be more so keep in mind)
    let old = GameAccountV1::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    
    let new_data = GameAccount {
        is_initialized: old.is_initialized,
        version: CURRENT_GAME_STATE_VERSION,
        game_title: old.game_title,
        owner: old.owner,
        dev_fee_bps: old.dev_fee_bps,
        game_to_protocol_share_bps: old.game_to_protocol_share_bps,
        game_cost: 0, // default to 0 for existing games, can be updated later by the game owner
        game_url: [0u8; 64], // default to empty for existing games, can be updated later by the game owner
        total_users: 0, // we don't have this data from the old version, so we initialize it to 0
        total_revenue_usd_micro: 0, // we don't have this data from the old version, so we initialize it to 0
    };

    // Grow the account to MAX SIZE
    let new_size = GameAccount::MAX_SIZE;
    if game_account.data_len() < new_size {
        let rent = Rent::get()?;
        let additional_lamports = rent.minimum_balance(new_size) - game_account.lamports();
        invoke_signed(
            &system_instruction::transfer(
                authority.key,
                game_account.key,
                additional_lamports,
            ),
            &[authority.clone(), game_account.clone()],
            &[],
        )?;
        game_account.realloc(new_size, false)?;
    }

    // Write the new data layout
    new_data.serialize(&mut &mut game_account.data.borrow_mut()[..])?;

    msg!(
        "Upgraded game state to version {} from version {} successfully",
        CURRENT_GAME_STATE_VERSION,
        old_version
    );

    Ok(())
}

pub fn developer_withdraw(
    accounts: &[AccountInfo],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let dev_wallet = next_account_info(iter)?; // game owner & signer
    let game_account = next_account_info(iter)?; // PDA (owns the source ATAs)
    let game_skr_ata = next_account_info(iter)?;
    let game_slice_ata = next_account_info(iter)?;
    let game_usdc_ata = next_account_info(iter)?;
    let destination_skr_ata = next_account_info(iter)?;
    let destination_slice_ata = next_account_info(iter)?;
    let destination_usdc_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !dev_wallet.is_signer {
        msg!("Developer wallet must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // -- Validate the game account ------------------------------------------------
    if game_account.owner != &PROGRAM_ID {
        msg!("Game account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let game_data = GameAccount::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if !game_data.is_initialized {
        msg!("Game account not initialized");
        return Err(ProgramError::UninitializedAccount);
    }

    if game_data.owner != *dev_wallet.key {
        msg!("Only the game owner can perform a developer withdraw");
        return Err(ProgramError::IllegalOwner);
    }

    // Re-derive the game PDA + bump from its title so it can sign the transfers.
    let (game_pda, game_bump) = find_game_pda(&game_data.game_title);
    if game_pda != *game_account.key {
        msg!("Game account PDA derivation mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let signer_seeds: &[&[u8]] = &[b"igp_game", &game_data.game_title, &[game_bump]];

    // -- Withdraw each pool in full to its destination ----------------------------
    withdraw_pool(
        game_account, game_skr_ata, destination_skr_ata,
        &PaymentToken::SKR, token_program, signer_seeds, "SKR",
    )?;
    withdraw_pool(
        game_account, game_slice_ata, destination_slice_ata,
        &PaymentToken::SLICE, token_program, signer_seeds, "SLICE",
    )?;
    withdraw_pool(
        game_account, game_usdc_ata, destination_usdc_ata,
        &PaymentToken::USDC, token_program, signer_seeds, "USDC",
    )?;

    // After the three SPL withdrawals, sweep excess SOL (above rent-exemption)
    // from the game PDA to the dev wallet.
    let rent = Rent::get()?;
    let min_rent = rent.minimum_balance(game_account.data_len());
    let game_lamports = game_account.lamports();
    let withdrawable = game_lamports.saturating_sub(min_rent);

    if withdrawable > 0 {
        **game_account.try_borrow_mut_lamports()? -= withdrawable;
        **dev_wallet.try_borrow_mut_lamports()? += withdrawable;
        msg!("Withdrew {} excess SOL lamports to dev wallet", withdrawable);
    } else {
        msg!("No excess SOL to withdraw");
    }

    msg!("Developer withdraw complete for game '{}'", String::from_utf8_lossy(&game_data.game_title));
    Ok(())
}

/// Transfer the entire balance of one game-owned ATA to a destination ATA.
/// No-op if the source is empty / uninitialized.
fn withdraw_pool<'a>(
    game_account: &AccountInfo<'a>,
    source_ata: &AccountInfo<'a>,
    destination_ata: &AccountInfo<'a>,
    token: &PaymentToken,
    token_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
    label: &str,
) -> ProgramResult {
    let mint = token.mint();

    // Verify the source is the game's canonical ATA for this mint.
    let expected_source = get_associated_token_address(game_account.key, &mint);
    if expected_source != *source_ata.key {
        msg!("{} source ATA address mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }

    // Read the source balance (full SPL Token account parse for safety).
    if source_ata.data_is_empty() {
        msg!("{} source ATA does not exist, skipping", label);
        return Ok(());
    }
    let source_state = SplTokenAccount::unpack(&source_ata.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;

    // Sanity: the source ATA must actually be owned by the game PDA.
    if source_state.owner != *game_account.key {
        msg!("{} source ATA not owned by game PDA", label);
        return Err(ProgramError::InvalidAccountData);
    }
    if source_state.mint != mint {
        msg!("{} source ATA mint mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }

    let amount = source_state.amount;
    if amount == 0 {
        msg!("{} pool empty, nothing to withdraw", label);
        return Ok(());
    }

    invoke_signed(
        &transfer(
            token_program.key,
            source_ata.key,
            destination_ata.key,
            game_account.key, // authority = game PDA (owns the source ATA)
            &[],
            amount,
        )?,
        &[
            source_ata.clone(),
            destination_ata.clone(),
            game_account.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )?;

    msg!("{} withdrew {} base units to destination", label, amount);
    Ok(())
}

pub fn reward_user_account(
    accounts: &[AccountInfo],
    percentage_of_the_pool: u16, // 0..=10000 bps of each protocol pool
) -> ProgramResult {
    let iter = &mut accounts.iter();

    let authority = next_account_info(iter)?;       // protocol authority & signer
    let game_account = next_account_info(iter)?;    // game PDA (for game_title -> protocol PDA)
    let game_protocol = next_account_info(iter)?;   // game protocol PDA (owns the source ATAs + holds SOL)
    let user_account = next_account_info(iter)?;    // target user's UserAccount PDA
    let oracle_account = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    // Per-token: (protocol source ATA, user destination ATA). USDC, SKR, SLICE order.
    let protocol_usdc_ata = next_account_info(iter)?;
    let user_usdc_ata = next_account_info(iter)?;
    let protocol_skr_ata = next_account_info(iter)?;
    let user_skr_ata = next_account_info(iter)?;
    let protocol_slice_ata = next_account_info(iter)?;
    let user_slice_ata = next_account_info(iter)?;

    assert_authority(authority)?;

    if percentage_of_the_pool as u64 > BPS_DENOMINATOR {
        msg!("percentage_of_the_pool exceeds 10000 (100%)");
        return Err(ProgramError::InvalidInstructionData);
    }

    // -- Validate the game account (to derive the protocol PDA) -------------------
    if game_account.owner != &PROGRAM_ID {
        msg!("Game account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let game_data = GameAccount::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if !game_data.is_initialized {
        msg!("Game account not initialized");
        return Err(ProgramError::UninitializedAccount);
    }

    // -- Validate + derive the game protocol PDA ----------------------------------
    let (gp_pda, gp_bump) = find_game_protocol_pda(&game_data.game_title);
    if gp_pda != *game_protocol.key {
        msg!("Game protocol PDA mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    if game_protocol.owner != &PROGRAM_ID {
        msg!("Game protocol account not owned by program");
        return Err(ProgramError::IllegalOwner);
    }
    let signer_seeds: &[&[u8]] = &[b"igp_game_protocol", &game_data.game_title, &[gp_bump]];

    // -- Validate the target user account -----------------------------------------
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
    if user_data.game != *game_account.key {
        msg!("User account game mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // -- Load oracle for USD reward accounting ------------------------------------
    let oracle = load_oracle(oracle_account)?;
    let mut total_paid_usd_micro: u64 = 0;

    // -- Reward a % of each protocol pool to the user account ---------------------
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(reward_from_protocol_pool(
        game_protocol, protocol_usdc_ata, user_usdc_ata, user_account,
        token_program, &oracle, PaymentToken::USDC, signer_seeds,
        percentage_of_the_pool, "USDC",
    )?);
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(reward_from_protocol_pool(
        game_protocol, protocol_skr_ata, user_skr_ata, user_account,
        token_program, &oracle, PaymentToken::SKR, signer_seeds,
        percentage_of_the_pool, "SKR",
    )?);
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(reward_from_protocol_pool(
        game_protocol, protocol_slice_ata, user_slice_ata, user_account,
        token_program, &oracle, PaymentToken::SLICE, signer_seeds,
        percentage_of_the_pool, "SLICE",
    )?);

    // -- Reward a % of the protocol PDA's surplus native SOL to the user account --
    total_paid_usd_micro = total_paid_usd_micro.saturating_add(reward_native_sol(
        game_protocol, user_account, &oracle, percentage_of_the_pool,
    )?);

    // -- Credit the user's reward accounting --------------------------------------
    user_data.total_usd_rewards_micro = user_data
        .total_usd_rewards_micro
        .checked_add(total_paid_usd_micro)
        .ok_or(ProgramError::InvalidAccountData)?;
    user_data.serialize(&mut &mut user_account.data.borrow_mut()[..])?;

    msg!(
        "Rewarded user account {} | {} bps of each protocol pool | ~{} USD micro, protocol pda: {}, game pda: {}",
        user_account.key,
        percentage_of_the_pool,
        total_paid_usd_micro,
        game_protocol.key,
        game_account.key
    );
    Ok(())
}

/// Transfer `bps` of one protocol-owned pool to the user's ATA. Returns USD micro paid.
#[allow(clippy::too_many_arguments)]
fn reward_from_protocol_pool<'a>(
    game_protocol: &AccountInfo<'a>,
    source_ata: &AccountInfo<'a>,
    dest_ata: &AccountInfo<'a>,
    user_account: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    oracle: &crate::state::oracle::OraclePriceState,
    token: PaymentToken,
    signer_seeds: &[&[u8]],
    bps: u16,
    label: &str,
) -> Result<u64, ProgramError> {
    let mint = token.mint();

    if get_associated_token_address(game_protocol.key, &mint) != *source_ata.key {
        msg!("{} protocol source ATA mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }
    if get_associated_token_address(user_account.key, &mint) != *dest_ata.key {
        msg!("{} user destination ATA mismatch", label);
        return Err(ProgramError::InvalidAccountData);
    }

    if source_ata.data_is_empty() {
        return Ok(0);
    }
    let source_state = SplTokenAccount::unpack(&source_ata.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if source_state.amount == 0 {
        return Ok(0);
    }

    let amount = ((source_state.amount as u128)
        .saturating_mul(bps as u128)
        / BPS_DENOMINATOR as u128) as u64;
    if amount == 0 {
        return Ok(0);
    }

    invoke_signed(
        &transfer(
            token_program.key,
            source_ata.key,
            dest_ata.key,
            game_protocol.key, // authority = protocol PDA (owns the source ATA)
            &[],
            amount,
        )?,
        &[
            source_ata.clone(),
            dest_ata.clone(),
            game_protocol.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )?;

    let decimals = token_decimals(&token);
    let price_micro_usd = price_for(oracle, &token);
    let paid = (amount as u128)
        .checked_mul(price_micro_usd as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?
        .checked_div(10_u128.pow(decimals as u32))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let paid = paid.min(u64::MAX as u128) as u64;

    msg!("{} rewarded {} base units (~{} USD micro)", label, amount, paid);
    Ok(paid)
}

/// Pay `bps` of the protocol PDA's *surplus* native lamports (above rent-exemption)
/// to the user wallet. Returns USD micro paid. Never drops the PDA below rent.
fn reward_native_sol<'a>(
    game_protocol: &AccountInfo<'a>,
    user_account: &AccountInfo<'a>,
    oracle: &crate::state::oracle::OraclePriceState,
    bps: u16,
) -> Result<u64, ProgramError> {
    let rent = Rent::get()?;
    let min_balance = rent.minimum_balance(game_protocol.data_len());

    let current = game_protocol.lamports();

    // Everything at or below the rent floor is untouchable.
    let spendable = current.saturating_sub(min_balance);
    if spendable == 0 {
        return Ok(0);
    }

    let amount = ((spendable as u128)
        .saturating_mul(bps as u128)
        / BPS_DENOMINATOR as u128) as u64;
    if amount == 0 {
        return Ok(0);
    }

    // Re-check the floor against the actual debit. Post-transfer must stay rent-exempt.
    let new_balance = current
        .checked_sub(amount)
        .ok_or(ProgramError::InsufficientFunds)?;
    if new_balance < min_balance {
        msg!("Native reward would drop protocol PDA below rent exemption");
        return Err(ProgramError::InsufficientFunds);
    }

    // PDA is program-owned: move lamports by direct field mutation, not a CPI transfer.
    **game_protocol.try_borrow_mut_lamports()? = new_balance;
    **user_account.try_borrow_mut_lamports()? = user_account
        .lamports()
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    // -- USD accounting (SOL = 9 decimals, no mint) -------------------------------
    let decimals: u8 = 9;
    let price_micro_usd = price_for(oracle, &PaymentToken::SOL);
    let paid = (amount as u128)
        .checked_mul(price_micro_usd as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?
        .checked_div(10_u128.pow(decimals as u32))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let paid = paid.min(u64::MAX as u128) as u64;

    msg!(
        "SOL rewarded {} lamports (~{} USD micro, rent floor {}, balance now {})",
        amount, paid, min_balance, new_balance
    );
    Ok(paid)
}