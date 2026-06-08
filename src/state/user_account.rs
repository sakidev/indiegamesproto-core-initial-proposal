use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

pub const USERNAME_MAX_LEN: usize = 32;

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct UserAccount {
    pub is_initialized: bool,
    pub version: u8,
    pub bump: u8,
    pub owner: Pubkey,
    // The game PDA this user account is associated with
    pub game: Pubkey,
    pub username: [u8; USERNAME_MAX_LEN],
    // The ranked game PDA the user is currently playing (if any)
    pub current_ranked_game: Pubkey,
    pub total_ranked_games_played: u32,
    pub total_wins: u32,
    pub total_usd_spent_micro: u64,
    pub total_usd_rewards_micro: u64,
}

impl UserAccount {
    // is_initialized + version + bump + owner + game + username + current_ranked_game + total_ranked_games_played + total_wins + total_usd_spent_micro + total_usd_rewards_micro = 146 bytes
    pub const MAX_SIZE: usize = 1 + 1 + 1 + 32 + 32 + USERNAME_MAX_LEN + 32 + 4 + 4 + 8 + 8;
}