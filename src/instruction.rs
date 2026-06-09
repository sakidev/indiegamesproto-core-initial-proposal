use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

use crate::constants::*;

// Every instruction the Indie Games Protocol accepts must be defined in this enum.
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum IndieGamesInstruction {
    // -- Oracle ----------------------------------------------
    InitOracle {
        sol_price_usd_micro_per_lamport: u64,
        skr_price_usd_micro_per_atom: u64,
        slice_price_usd_micro_per_atom: u64,
    },

    UpdateOraclePrices {
        sol_price_usd_micro_per_lamport: u64,
        skr_price_usd_micro_per_atom: u64,
        slice_price_usd_micro_per_atom: u64,
    },



    // -- Game Title ------------------------------------------
    CreateGame {
        title: [u8; 32],
        dev_fee_bps: u16,
        game_to_protocol_share_bps: u16,
        game_cost: u64,
        game_url: [u8; 64],
    },

    UpdateFeeShare {
        new_dev_fees_bps: u16,
        new_game_to_protocol_share_bps: u16,
    },

    UpdateGameCost {
        new_game_cost: u64,
    },

    TransferGameOwnership {
        new_owner: Pubkey,
    },

    /// Special instruction to upgrade the data layout of GameAccounts when we need to add new fields, this way we can maintain backwards compatibility and not break existing games when we want to add new features
    UpgradeGameStateVersion,

    // The Game developer can withdraw the entirety of his earnings
    DeveloperWithdraw,

    RewardUserAccount {
        percentage_of_the_pool: u16,
    },


    // -- User Account -----------------------------------------
    CreateUserAccount {
        username: [u8; 32],
        /// The current timestamp in seconds, this way users can create an infinite number of accounts (and thus support an accounts/characters marketplace in the future)
        timestamp: u64,
        /// The token the user wants to pay with (e.g. SOL, SKR, SLICE, USDC), this is needed to determine the price using the oracle and transfer the funds to the game account
        payment_token: Pubkey,
    },

    WithdrawRewards {
        // The timestamp
        timestamp: u64,
    },



    // -- Ranked Game -----------------------------------------
    CreateRankedGame {
        ranked_game_id: u64,
        entry_fee_usd_micro: u64,
    },

    JoinRankedGame {
        ranked_game_id: u64,
        payment_token: Pubkey,
    },

    UpdateRankedGameStatus {
        ranked_game_id: u64,
        new_status: u8,
    },

    RewardWinner {
        ranked_game_id: u64,
        amount_usd_micro: u16,
    },

    CloseRankedGame {
        ranked_game_id: u64,
    },
}