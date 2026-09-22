//! Minimal instruction builders for the Reflect Proxy Program.
//! Only includes Wrap and Unwrap instructions needed for the trading venue.
//!
//! Account layout matches the deployed `reflect-companion-program` (`main`):
//! `[user, stablecoin_user_ata, branded_user_ata, proxy_state, stablecoin_vault,
//!   branded_mint, oracle, asset, token_program, stablecoin_mint]`.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::REFLECT_PROXY_PROGRAM_ID;

const WRAP_DISCRIMINATOR: u8 = 1;
const UNWRAP_DISCRIMINATOR: u8 = 2;

#[derive(Debug)]
pub struct Wrap {
    pub user: Pubkey,
    pub stablecoin_user_token_account: Pubkey,
    pub branded_user_token_account: Pubkey,
    pub proxy_state: Pubkey,
    pub stablecoin_proxy_state_vault: Pubkey,
    pub branded_mint: Pubkey,
    pub oracle: Pubkey,
    /// Asset registry PDA (`["asset", stablecoin_mint]`). The program reads the
    /// oracle/feed identity from it.
    pub asset: Pubkey,
    pub token_program: Pubkey,
    /// Stablecoin mint, required for the checked SPL transfer into the vault.
    pub stablecoin_mint: Pubkey,
    /// Trailing oracle-chain legs (empty for single-leg assets). Appended after
    /// `stablecoin_mint`; the program reads them via `extra_oracles @ ..`.
    pub extra_oracles: Vec<Pubkey>,
}

pub struct WrapInstructionArgs {
    pub amount: u64,
    pub min_branded_tokens: u64,
}

impl Wrap {
    pub fn instruction(&self, args: WrapInstructionArgs) -> Instruction {
        let mut accounts = vec![
            AccountMeta::new_readonly(self.user, true),
            AccountMeta::new(self.stablecoin_user_token_account, false),
            AccountMeta::new(self.branded_user_token_account, false),
            AccountMeta::new(self.proxy_state, false),
            AccountMeta::new(self.stablecoin_proxy_state_vault, false),
            AccountMeta::new(self.branded_mint, false),
            AccountMeta::new_readonly(self.oracle, false),
            AccountMeta::new_readonly(self.asset, false),
            AccountMeta::new_readonly(self.token_program, false),
            AccountMeta::new_readonly(self.stablecoin_mint, false),
        ];
        accounts.extend(
            self.extra_oracles
                .iter()
                .map(|oracle| AccountMeta::new_readonly(*oracle, false)),
        );

        let mut data = Vec::with_capacity(17);
        data.push(WRAP_DISCRIMINATOR);
        data.extend_from_slice(&args.amount.to_le_bytes());
        data.extend_from_slice(&args.min_branded_tokens.to_le_bytes());

        Instruction {
            program_id: REFLECT_PROXY_PROGRAM_ID,
            accounts,
            data,
        }
    }
}

#[derive(Debug)]
pub struct Unwrap {
    pub user: Pubkey,
    pub stablecoin_user_token_account: Pubkey,
    pub branded_user_token_account: Pubkey,
    pub proxy_state: Pubkey,
    pub stablecoin_proxy_state_vault: Pubkey,
    pub branded_mint: Pubkey,
    pub oracle: Pubkey,
    /// Asset registry PDA (`["asset", stablecoin_mint]`).
    pub asset: Pubkey,
    pub token_program: Pubkey,
    /// Stablecoin mint, required for the checked SPL transfer out of the vault.
    pub stablecoin_mint: Pubkey,
    /// Trailing oracle-chain legs (empty for single-leg assets).
    pub extra_oracles: Vec<Pubkey>,
}

pub struct UnwrapInstructionArgs {
    pub amount: u64,
    pub min_usdc_plus: u64,
}

impl Unwrap {
    pub fn instruction(&self, args: UnwrapInstructionArgs) -> Instruction {
        let mut accounts = vec![
            AccountMeta::new_readonly(self.user, true),
            AccountMeta::new(self.stablecoin_user_token_account, false),
            AccountMeta::new(self.branded_user_token_account, false),
            AccountMeta::new(self.proxy_state, false),
            AccountMeta::new(self.stablecoin_proxy_state_vault, false),
            AccountMeta::new(self.branded_mint, false),
            AccountMeta::new_readonly(self.oracle, false),
            AccountMeta::new_readonly(self.asset, false),
            AccountMeta::new_readonly(self.token_program, false),
            AccountMeta::new_readonly(self.stablecoin_mint, false),
        ];
        accounts.extend(
            self.extra_oracles
                .iter()
                .map(|oracle| AccountMeta::new_readonly(*oracle, false)),
        );

        let mut data = Vec::with_capacity(17);
        data.push(UNWRAP_DISCRIMINATOR);
        data.extend_from_slice(&args.amount.to_le_bytes());
        data.extend_from_slice(&args.min_usdc_plus.to_le_bytes());

        Instruction {
            program_id: REFLECT_PROXY_PROGRAM_ID,
            accounts,
            data,
        }
    }
}
