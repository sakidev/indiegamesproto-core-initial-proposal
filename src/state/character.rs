use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

pub const CHARACTER_NAME_MAX_LEN: usize = 32;

/// On-chain data for a character account.
/// PDA seeds: ["character", owner_wallet, character_name]
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct CharacterAccount {
    pub is_initialized: bool,
    pub version: u8,
    pub owner_wallet: Pubkey,
    /// Unix timestamp set at creation.
    pub creation_date: i64,
    pub character_name: String,
    pub level: u32,
    pub ranked_games_played: u32,
    pub ranked_games_won: u32,
    /// Set to the ranked game PDA while the character is enrolled; None otherwise.
    pub current_ranked_game: Option<Pubkey>,
}

impl CharacterAccount {
    /// Maximum serialized byte size.
    /// Layout:
    ///   is_initialized(1) + version(1) + owner_wallet(32) + creation_date(8)
    ///   + string_len(4) + character_name(32)
    ///   + level(4) + ranked_games_played(4) + ranked_games_won(4)
    ///   + option_tag(1) + current_ranked_game(32)
    pub const MAX_SIZE: usize =
        1 + 1 + 32 + 8 + 4 + CHARACTER_NAME_MAX_LEN + 4 + 4 + 4 + 1 + 32;
}
