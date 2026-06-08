use solana_program::pubkey::{pubkey, Pubkey};
use borsh::{BorshDeserialize, BorshSerialize};

pub const PROGRAM_ID: Pubkey = pubkey!("iGPa16mPdKghdCffhHyXs5HBUAZP7EvJohpGbQgnBiv");
pub const AUTHORITY: Pubkey = pubkey!("iGPbBHvkpLmVUWVExoGspENmjVr5As2zSRavnmR811h");

// 10% is the MINIMUM the PROTOCOL will accept from a GAME.
// This is to ensure the PROTOCOL can sustain itself and continue to operate.
// Games can choose to pay more than this, but not less.
pub const MIN_GAME_TO_PROTOCOL_SHARE_BPS: u16 = 1_000;

// SPL token mints
pub const SKR_MINT: Pubkey = pubkey!("SKRbvo6Gf7GondiT3BbTfuRDPqLWei4j2Qy2NPGZhW3");
pub const SLICE_MINT: Pubkey = pubkey!("SLiCEp8HFG2E3Hai7XB7UKtgbd7heUgqn5tV7zfBJNX");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

// Account discriminators
pub const RANKED_GAME_DISCRIMINATOR: [u8; 8]    = *b"RNKDGAME";
pub const ORACLE_DISCRIMINATOR: [u8; 8]         = *b"ORACLE11";

// Increment this whenever we make breaking changes to the GameAccount struct
pub const CURRENT_GAME_STATE_VERSION: u8 = 2;

// Payment tokens (supported SPL mints)
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub enum PaymentToken {
    // Native SOL (lamports)
    SOL,
    // Solana Mobile Seeker Token
    SKR,
    // Protocol native SLICE token
    SLICE,
    // USDC stablecoin
    USDC
}

impl PaymentToken {
    pub fn mint(&self) -> Pubkey {
        match self {
            PaymentToken::SOL => pubkey!("So11111111111111111111111111111111111111112"),
            PaymentToken::SKR => SKR_MINT,
            PaymentToken::SLICE => SLICE_MINT,
            PaymentToken::USDC => USDC_MINT,
        }
    }
}