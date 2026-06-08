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
};
use spl_token::state::Account as SplTokenAccount;
use spl_associated_token_account::get_associated_token_address;


use crate::{
    constants::{PROGRAM_ID, CURRENT_GAME_STATE_VERSION, PaymentToken},
    instruction::IndieGamesInstruction,
    helpers::{validate_dev_fee_bps, find_game_pda, find_user_account_pda, find_game_protocol_pda, assert_authority},
    state::{
        game::{GameAccountV1, GameAccount, GAME_TITLE_MAX_LEN},
    }
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