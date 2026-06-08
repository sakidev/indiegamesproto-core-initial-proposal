use solana_program::program_error::ProgramError;
use thiserror::Error;

#[derive(Error, Debug, Copy, Clone)]
pub enum IndieGamesError {
    #[error("Invalid instruction data")]
    InvalidInstruction,
    #[error("Account is already initialized")]
    AlreadyInitialized,
    #[error("Ranked game must be in Created state")]
    RankedGameNotCreated,
    #[error("Ranked game must be in Running state")]
    RankedGameNotRunning,
    #[error("Ranked game must be in Finished state")]
    RankedGameNotFinished,
    #[error("Character is already enrolled in a ranked game")]
    CharacterAlreadyInGame,
    #[error("Unauthorized: signer is not the admin or owner of this account")]
    Unauthorized,
    #[error("Provided account does not match the expected PDA")]
    InvalidPDA,
    #[error("Arithmetic overflow")]
    ArithmeticOverflow,
    #[error("Insufficient lamports in the ranked game account")]
    InsufficientFunds,
    #[error("Total payout amounts exceed the ranked game pot")]
    PayoutExceedsPot,
    #[error("String exceeds the maximum allowed length")]
    StringTooLong,
    #[error("Ranked game has reached the maximum number of participants")]
    MaxParticipantsReached,
}

impl From<IndieGamesError> for ProgramError {
    fn from(e: IndieGamesError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
