use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

// -- Ranked game status ------------------------------------------

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub enum RankedGameStatus {
    /// Accepting new participants.
    Open,
    /// No more joins; match is in progress.
    InProgress,
    /// Match is over; payouts may be processed.
    Finished,
}

// -- Ranked game account ------------------------------------------

/// On-chain data stored inside a ranked game PDA.
///
/// PDA seeds: [b"igp_ranked_game", ranked_game_id]
///
/// AUTHORITY is not stored - it is the hard-coded constant.
///
/// The `game` field links this ranked game to its parent game PDA.
/// All protocol fees (10 % of each entry) are routed to that game PDA,
/// accumulating as SOL lamports or SPL token balances on its ATAs.
///
/// Layout:
///   discriminator(8) + ranked_game_id(32) + game(32) + entry_fee(8)
///   + participant_count(4) + status(1) + bump(1) = 86 bytes
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct RankedGameState {
    pub discriminator: [u8; 8],
    /// Unique ID used as PDA seed (up to 32 bytes, zero-padded by the caller).
    pub ranked_game_id: u64,
    /// Parent game PDA — receives all protocol fees accrued by this ranked game.
    pub game: Pubkey,
    /// Net entry fee per participant in USD cents (e.g. 100 = $1.00).
    /// The 10 % protocol fee is charged on top of this at join time and routed to `game`.
    pub entry_fee: u64,
    pub participant_count: u32,
    pub status: RankedGameStatus,
    /// Canonical PDA bump cached to save compute in later instructions.
    pub bump: u8,
}

impl RankedGameState {
    pub const LEN: usize = 8 + 8 + 32 + 8 + 4 + 1 + 1; // 62
}