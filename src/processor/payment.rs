use borsh::BorshDeserialize;
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    msg,
    program::invoke,
    program_error::ProgramError,
    system_instruction,
};
use spl_token::instruction::transfer_checked;

use crate::{
    constants::PaymentToken,
    helpers::{load_oracle, split_protocol_fee, usd_cents_to_base_units, token_decimals, RANKED_FEE_BPS, BPS_DENOMINATOR},
    state::game::GameAccount,
};

/// SPL token accounts required for a non-SOL payment.
pub struct SplPaymentAccounts<'a> {
    pub payer_ata: &'a AccountInfo<'a>,
    pub game_ata: &'a AccountInfo<'a>,
    pub protocol_ata: &'a AccountInfo<'a>,
    pub mint: &'a AccountInfo<'a>,
}

/// Charge `cost_usd_micro` (USD, micro = 1e-6) from the payer, splitting it
/// between the game PDA and the protocol treasury per the game's configured
/// `game_to_protocol_share_bps`.
///
/// For SOL, transfers are native system transfers. For SPL tokens, the caller
/// must supply `spl_accounts`. The USD cost is converted to token base units
/// via the on-chain oracle.
pub fn process_payment<'a>(
    payer: &AccountInfo<'a>,
    game_account: &AccountInfo<'a>,
    game_protocol: &AccountInfo<'a>,
    oracle_account: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    spl_accounts: Option<SplPaymentAccounts<'a>>,
    cost_usd_micro: u64,
    payment_token: PaymentToken,
) -> ProgramResult {
    if !payer.is_signer {
        msg!("Payer must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    let game = GameAccount::try_from_slice(&game_account.data.borrow())
        .map_err(|_| ProgramError::InvalidAccountData)?;
    if !game.is_initialized {
        msg!("Game account is not initialized");
        return Err(ProgramError::UninitializedAccount);
    }

    // helpers operate in USD cents; game_cost is micro-USD (1e-6). Convert.
    let usd_cents = cost_usd_micro / 10_000;

    // load_oracle verifies owner == PROGRAM_ID, canonical PDA, and discriminator.
    let oracle = load_oracle(oracle_account)?;

    let (price, decimals) = match payment_token {
        PaymentToken::SOL => (oracle.sol_price_usd_micro_per_lamport, 9u8),
        PaymentToken::SKR => (oracle.skr_price_usd_micro_per_atom, 6u8),
        PaymentToken::SLICE => (oracle.slice_price_usd_micro_per_atom, 6u8),
        // 1 USDC = 1e6 micro-USD; identity through the same formula.
        PaymentToken::USDC => (1_000_000u64, 6u8),
    };

    let total = usd_cents_to_base_units(usd_cents, price, decimals)?;

    // split_protocol_fee returns (dev/game amount, protocol amount).
    let (game_amount, protocol_amount) =
        split_protocol_fee(total, game.game_to_protocol_share_bps);

    if payment_token == PaymentToken::SOL {
        invoke(
            &system_instruction::transfer(payer.key, game_account.key, game_amount),
            &[payer.clone(), game_account.clone(), system_program.clone()],
        )?;
        invoke(
            &system_instruction::transfer(payer.key, game_protocol.key, protocol_amount),
            &[payer.clone(), game_protocol.clone(), system_program.clone()],
        )?;
    } else {
        let spl = spl_accounts.ok_or(ProgramError::NotEnoughAccountKeys)?;

        if spl.mint.key != &payment_token.mint() {
            msg!("Wrong mint for payment token {:?}", payment_token);
            return Err(ProgramError::InvalidAccountData);
        }

        invoke(
            &transfer_checked(
                token_program.key,
                spl.payer_ata.key,
                spl.mint.key,
                spl.game_ata.key,
                payer.key,
                &[],
                game_amount,
                decimals,
            )?,
            &[
                spl.payer_ata.clone(),
                spl.mint.clone(),
                spl.game_ata.clone(),
                payer.clone(),
                token_program.clone(),
            ],
        )?;
        invoke(
            &transfer_checked(
                token_program.key,
                spl.payer_ata.key,
                spl.mint.key,
                spl.protocol_ata.key,
                payer.key,
                &[],
                protocol_amount,
                decimals,
            )?,
            &[
                spl.payer_ata.clone(),
                spl.mint.clone(),
                spl.protocol_ata.clone(),
                payer.clone(),
                token_program.clone(),
            ],
        )?;
    }

    msg!(
        "Payment: {} base units ({} usd_micro) — game {} / protocol {}",
        total,
        cost_usd_micro,
        game_amount,
        protocol_amount
    );
    Ok(())
}



// -- RANKED GAMES --------------------------------------------------------------

/// Destination ATAs for an SPL ranked-game entry.
pub struct RankedSplPaymentAccounts<'a> {
    pub payer_ata:       &'a AccountInfo<'a>,
    pub ranked_game_ata: &'a AccountInfo<'a>, // receives the NET entry (escrow)
    pub game_ata:        &'a AccountInfo<'a>, // receives the dev cut of the fee
    pub protocol_ata:    &'a AccountInfo<'a>, // receives the protocol cut of the fee
    pub mint:            &'a AccountInfo<'a>,
}

#[allow(clippy::too_many_arguments)]
pub fn process_ranked_entry_payment<'a>(
    payer: &AccountInfo<'a>,
    ranked_game_account: &AccountInfo<'a>,
    game_account: &AccountInfo<'a>,
    game_protocol: &AccountInfo<'a>,
    oracle_account: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    spl_accounts: Option<RankedSplPaymentAccounts<'a>>,
    entry_fee: u64,                  // net entry, USD cents
    dev_fee_bps: u16,                // dev's share OF the fee
    game_to_protocol_share_bps: u16, // protocol's share OF the fee
    payment_token: PaymentToken,
) -> ProgramResult {
    if dev_fee_bps
        .checked_add(game_to_protocol_share_bps)
        .ok_or(ProgramError::ArithmeticOverflow)?
        != BPS_DENOMINATOR as u16
    {
        msg!("dev_fee_bps + game_to_protocol_share_bps must equal 10000");
        return Err(ProgramError::InvalidInstructionData);
    }

    let dev_bps = dev_fee_bps as u64;

    // Fee is 10% ON TOP of the net entry (all in USD cents).
    let fee = entry_fee
        .checked_mul(RANKED_FEE_BPS)
        .ok_or(ProgramError::ArithmeticOverflow)?
        / BPS_DENOMINATOR;

    let dev_cut = fee
        .checked_mul(dev_bps)
        .ok_or(ProgramError::ArithmeticOverflow)?
        / BPS_DENOMINATOR;
    // Rounding dust goes to protocol.
    let protocol_cut = fee.checked_sub(dev_cut).ok_or(ProgramError::ArithmeticOverflow)?;

    let total_due = entry_fee.checked_add(fee).ok_or(ProgramError::ArithmeticOverflow)?;

    // -- Convert each USD-cent component -> token base units via oracle ------------------------------------------------
    let decimals = token_decimals(&payment_token);
    let oracle = load_oracle(oracle_account)?;
    let price = match payment_token {
        PaymentToken::SOL   => oracle.sol_price_usd_micro_per_lamport,
        PaymentToken::SKR   => oracle.skr_price_usd_micro_per_atom,
        PaymentToken::SLICE => oracle.slice_price_usd_micro_per_atom,
        PaymentToken::USDC  => 1_000_000u64, // 1 USD per USDC
    };

    let net_tok      = usd_cents_to_base_units(entry_fee, price, decimals)?;
    let dev_tok      = usd_cents_to_base_units(dev_cut, price, decimals)?;
    let protocol_tok = usd_cents_to_base_units(protocol_cut, price, decimals)?;

    match payment_token {
        PaymentToken::SOL => {
            // payer -> ranked_game_account (net)
            invoke(
                &system_instruction::transfer(payer.key, ranked_game_account.key, net_tok),
                &[payer.clone(), ranked_game_account.clone(), system_program.clone()],
            )?;
            // payer -> game_account (dev cut)
            if dev_tok > 0 {
                invoke(
                    &system_instruction::transfer(payer.key, game_account.key, dev_tok),
                    &[payer.clone(), game_account.clone(), system_program.clone()],
                )?;
            }
            // payer -> game_protocol (protocol cut)
            if protocol_tok > 0 {
                invoke(
                    &system_instruction::transfer(payer.key, game_protocol.key, protocol_tok),
                    &[payer.clone(), game_protocol.clone(), system_program.clone()],
                )?;
            }
        }
        _ => {
            let spl = spl_accounts.ok_or(ProgramError::NotEnoughAccountKeys)?;

            // Validate mint matches the chosen token.
            if spl.mint.key != &payment_token.mint() {
                msg!("Wrong mint for {:?}", payment_token);
                return Err(ProgramError::InvalidAccountData);
            }

            // Validate each destination ATA is canonical for its owner+mint.
            let expected_escrow = spl_associated_token_account::get_associated_token_address(
                ranked_game_account.key, spl.mint.key,
            );
            if spl.ranked_game_ata.key != &expected_escrow {
                msg!("Wrong escrow ATA");
                return Err(ProgramError::InvalidAccountData);
            }
            let expected_game = spl_associated_token_account::get_associated_token_address(
                game_account.key, spl.mint.key,
            );
            if spl.game_ata.key != &expected_game {
                msg!("Wrong game (dev) ATA");
                return Err(ProgramError::InvalidAccountData);
            }
            let expected_protocol = spl_associated_token_account::get_associated_token_address(
                game_protocol.key, spl.mint.key,
            );
            if spl.protocol_ata.key != &expected_protocol {
                msg!("Wrong protocol ATA");
                return Err(ProgramError::InvalidAccountData);
            }

            // payer_ata -> ranked_game_ata (net) -- escrow
            invoke(
                &transfer_checked(
                    token_program.key, spl.payer_ata.key, spl.mint.key,
                    spl.ranked_game_ata.key, payer.key, &[], net_tok, decimals,
                )?,
                &[
                    spl.payer_ata.clone(), spl.mint.clone(),
                    spl.ranked_game_ata.clone(), payer.clone(), token_program.clone(),
                ],
            )?;
            // payer_ata -> game_ata (dev cut)
            if dev_tok > 0 {
                invoke(
                    &transfer_checked(
                        token_program.key, spl.payer_ata.key, spl.mint.key,
                        spl.game_ata.key, payer.key, &[], dev_tok, decimals,
                    )?,
                    &[
                        spl.payer_ata.clone(), spl.mint.clone(),
                        spl.game_ata.clone(), payer.clone(), token_program.clone(),
                    ],
                )?;
            }
            // payer_ata -> protocol_ata (protocol cut)
            if protocol_tok > 0 {
                invoke(
                    &transfer_checked(
                        token_program.key, spl.payer_ata.key, spl.mint.key,
                        spl.protocol_ata.key, payer.key, &[], protocol_tok, decimals,
                    )?,
                    &[
                        spl.payer_ata.clone(), spl.mint.clone(),
                        spl.protocol_ata.clone(), payer.clone(), token_program.clone(),
                    ],
                )?;
            }
        }
    }

    msg!(
        "Ranked entry: net {} -> escrow, fee {} = dev {} + protocol {} (USD cents), total due {}",
        entry_fee, fee, dev_cut, protocol_cut, total_due
    );
    Ok(())
}