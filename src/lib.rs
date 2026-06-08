pub mod constants;
pub mod error;
pub mod helpers;
pub mod instruction;
pub mod processor;
pub mod state;

#[cfg(not(feature = "no-entrypoint"))]
pub mod entrypoint;

solana_program::declare_id!("iGPa16mPdKghdCffhHyXs5HBUAZP7EvJohpGbQgnBiv");