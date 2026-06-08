use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct OraclePriceState {
    pub discriminator: [u8; 8], // "ORACLE11"
    // micro-USD per lamport  (= SOL_USD × 1_000_000 / 1_000_000_000)
    pub sol_price_usd_micro_per_lamport: u64,
    // micro-USD per SKR atom
    pub skr_price_usd_micro_per_atom: u64,
    // micro-USD per SLICE atom
    pub slice_price_usd_micro_per_atom: u64,
    // Canonical bump cached at initialization
    pub bump: u8,
}

impl OraclePriceState {
    pub const LEN: usize = 8 + 8 + 8 + 8 + 1; // discriminator + 3 prices + bump = 33 bytes
}