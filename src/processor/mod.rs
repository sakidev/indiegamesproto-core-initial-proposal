pub mod oracle;
pub mod game;
pub mod user_account;
pub mod ranked_game;
pub mod payment;

use borsh::BorshDeserialize;
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, msg, program_error::ProgramError,
    pubkey::Pubkey,
    system_instruction,
    program::invoke,
};

use crate::{
    constants::{PROGRAM_ID, PaymentToken},
    instruction::IndieGamesInstruction,
    helpers::{load_oracle, usd_cents_to_base_units, split_protocol_fee},
    state::game::GameAccount,
};
use spl_token::instruction::transfer_checked;

pub struct Processor;

impl Processor {
    pub fn process<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        instruction_data: &[u8],
    ) -> ProgramResult {
        if *program_id != PROGRAM_ID {
            msg!("Program ID mismatch");
            return Err(ProgramError::IncorrectProgramId);
        }

        let instruction = IndieGamesInstruction::try_from_slice(instruction_data)
            .map_err(|_| ProgramError::InvalidInstructionData)?;

        match instruction {
            // ── Oracle ───────────────────────────────────────────────────────
            IndieGamesInstruction::InitOracle {
                sol_price_usd_micro_per_lamport,
                skr_price_usd_micro_per_atom,
                slice_price_usd_micro_per_atom,
            } => {
                msg!("Instruction: InitOracle");
                oracle::init_oracle(
                    accounts,
                    sol_price_usd_micro_per_lamport,
                    skr_price_usd_micro_per_atom,
                    slice_price_usd_micro_per_atom,
                )
            }
            IndieGamesInstruction::UpdateOraclePrices {
                sol_price_usd_micro_per_lamport,
                skr_price_usd_micro_per_atom,
                slice_price_usd_micro_per_atom,
            } => {
                msg!("Instruction: UpdateOraclePrices");
                oracle::update_oracle_prices(
                    accounts,
                    sol_price_usd_micro_per_lamport,
                    skr_price_usd_micro_per_atom,
                    slice_price_usd_micro_per_atom,
                )
            }
            // -- GAME TITLE ------------------------------------------------------
            IndieGamesInstruction::CreateGame {
                title,
                dev_fee_bps,
                game_to_protocol_share_bps,
                game_cost,
                game_url
            } => {
                msg!("Instruction: CreateGame");
                game::create_game(accounts, 2, title, dev_fee_bps, game_to_protocol_share_bps, game_cost, game_url)
            }
            IndieGamesInstruction::UpdateFeeShare {
                new_dev_fees_bps,
                new_game_to_protocol_share_bps,
            } => {
                msg!("Instruction: UpdateFeeShare");
                game::update_fee_share(accounts, new_dev_fees_bps, new_game_to_protocol_share_bps)
            }
            IndieGamesInstruction::UpdateGameCost {
                new_game_cost
            } => {
                msg!("Instruction: UpdateGameCost");
                game::update_game_cost(accounts, new_game_cost)
            }
            IndieGamesInstruction::TransferGameOwnership {
                new_owner,
            } => {
                msg!("Instruction: TransferGameOwnership");
                game::transfer_game_ownership(accounts, new_owner)
            }
            IndieGamesInstruction::UpgradeGameStateVersion => {
                msg!("Instruction: UpgradeGameStateVersion");
                game::upgrade_game_version(accounts)
            }
            IndieGamesInstruction::DeveloperWithdraw => {
                msg!("Instruction: DeveloperWithdraw");
                game::developer_withdraw(accounts)
            }
            IndieGamesInstruction::RewardUserAccount {
                percentage_of_the_pool
            } => {
                msg!("Instruction: RewardUserAccount");
                game::reward_user_account(accounts, percentage_of_the_pool)
            }

            // -- USER ACCOUNT ------------------------------------------------------
            IndieGamesInstruction::CreateUserAccount {
                username,
                timestamp,
                payment_token
            } => {
                msg!("Instruction: CreateUserAccount");
                user_account::create_user_account(accounts, 1, username, timestamp, payment_token)
            }
            IndieGamesInstruction::WithdrawRewards {
                timestamp
            } => {
                msg!("Instruction: WithdrawRewards");
                user_account::withdraw_rewards(accounts, timestamp)
            }

            // -- RANKED GAME ------------------------------------------------------
            IndieGamesInstruction::CreateRankedGame {
                ranked_game_id,
                entry_fee_usd_micro
            } => {
                msg!("Instruction: CreateRankedGame");
                ranked_game::create_ranked_game(accounts, ranked_game_id, entry_fee_usd_micro)
            }

            IndieGamesInstruction::JoinRankedGame {
                ranked_game_id,
                payment_token
            } => {
                msg!("Instruction: JoinRankedGame");
                ranked_game::join_ranked_game(accounts, ranked_game_id, payment_token)
            }

            IndieGamesInstruction::UpdateRankedGameStatus {
                ranked_game_id,
                new_status
            } => {
                msg!("Instruction: UpdateRankedGameStatus");
                ranked_game::update_ranked_game_status(accounts, ranked_game_id, new_status)
            }

            IndieGamesInstruction::RewardWinner {
                ranked_game_id,
                amount_usd_micro
            } => {
                msg!("Instruction: RewardWinner");
                ranked_game::reward_winner(accounts, ranked_game_id, amount_usd_micro)
            }

            IndieGamesInstruction::CloseRankedGame {
                ranked_game_id
            } => {
                msg!("Instruction: CloseRankedGame");
                ranked_game::close_ranked_game(accounts, ranked_game_id)
            }
        }
    }
}