//! Reflect Whitelabel trading venue integration for Titan.
//!
//! This module implements wrap/unwrap functionality for Reflect's whitelabel
//! stablecoin system. Users can wrap USDC+ (stablecoin) into branded tokens
//! and unwrap branded tokens back to USDC+.

use ahash::HashSet;
use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_token_2022::{extension::StateWithExtensions, state::Mint};

use crate::{
    account_caching::AccountsCache,
    trading_venue::{
        error::TradingVenueError, protocol::PoolProtocol, token_info::TokenInfo,
        AddressLookupTableTrait, FromAccount, QuoteRequest, QuoteResult, TradingVenue,
    },
};

/// Program ID for the Reflect Proxy Program.
pub const REFLECT_PROXY_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ProxycrBkRh1S1241nBRkqrJTgTX1ginHDR8xTLjYSE");

/// Oracle ID used by the Reflect Proxy Program for USDC+/USDC price.
/// This is the Doppler oracle account.
pub const ORACLE_ID: Pubkey = Pubkey::new_from_array([
    0x05, 0xda, 0xfe, 0x59, 0x3a, 0xcb, 0xc6, 0xa6,
    0x8d, 0xad, 0x1f, 0x4f, 0xa6, 0xda, 0x23, 0x6b,
    0x4c, 0x78, 0x91, 0x5c, 0x3e, 0x1f, 0xab, 0xb1,
    0x20, 0x10, 0x27, 0x35, 0x44, 0xf5, 0xa2, 0xa1,
]);

/// Virtual shares offset for ERC4626-style inflation attack mitigation.
const VIRTUAL_SHARES: u64 = 1_000_000;

/// Virtual USDC assets offset for ERC4626-style inflation attack mitigation.
const VIRTUAL_ASSETS_USDC: u64 = 1_000_000;

/// On-chain ProxyState size: 32 + 32 + 2 + 8 + 8 + 32 + 1 = 115 bytes
const PROXY_STATE_LEN: usize = 115;

/// Oracle account size: slot(8) + price(8) + precision(1) = 17 bytes
const ORACLE_LEN: usize = 17;

/// Parsed proxy state data from on-chain account.
/// This matches the on-chain layout exactly:
/// - branded_mint: [u8; 32]      offset 0
/// - stablecoin_mint: [u8; 32]   offset 32
/// - fee: u16                     offset 64
/// - principal: u64               offset 66
/// - integrators_commission: u64  offset 74
/// - authority: [u8; 32]          offset 82
/// - bump: u8                     offset 114
#[derive(Clone, Copy, Debug)]
struct ParsedProxyState {
    pub branded_mint: Pubkey,
    pub stablecoin_mint: Pubkey,
    pub fee: u16,
    pub principal: u64,
    pub integrators_commission: u64,
    pub authority: Pubkey,
    pub bump: u8,
}

impl ParsedProxyState {
    /// Parse proxy state from raw account data bytes.
    fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < PROXY_STATE_LEN {
            return None;
        }

        let branded_mint = Pubkey::try_from(&data[0..32]).ok()?;
        let stablecoin_mint = Pubkey::try_from(&data[32..64]).ok()?;
        let fee = u16::from_le_bytes(data[64..66].try_into().ok()?);
        let principal = u64::from_le_bytes(data[66..74].try_into().ok()?);
        let integrators_commission = u64::from_le_bytes(data[74..82].try_into().ok()?);
        let authority = Pubkey::try_from(&data[82..114]).ok()?;
        let bump = data[114];

        Some(Self {
            branded_mint,
            stablecoin_mint,
            fee,
            principal,
            integrators_commission,
            authority,
            bump,
        })
    }
}

/// Keys required for wrap/unwrap operations derived from the proxy state.
#[derive(Clone, Copy, Debug)]
pub struct ReflectWhitelabelKeys {
    /// The proxy state account (market identifier).
    pub proxy_state: Pubkey,
    /// The branded (whitelabel) token mint.
    pub branded_mint: Pubkey,
    /// The stablecoin (USDC+) mint.
    pub stablecoin_mint: Pubkey,
    /// The authority of the proxy.
    pub authority: Pubkey,
    /// The oracle account for USDC+/USDC price.
    pub oracle: Pubkey,
    /// The token program for the stablecoin.
    pub stablecoin_token_program: Pubkey,
    /// The vault holding stablecoin deposits.
    pub stablecoin_vault: Pubkey,
    /// The bump seed for the proxy state PDA.
    pub bump: u8,
}

/// Trading venue implementation for Reflect Whitelabel wrap/unwrap operations.
///
/// This venue allows swapping between a stablecoin (USDC+) and its branded
/// whitelabel version. The exchange rate is determined by the principal/supply
/// ratio with virtual offsets to prevent inflation attacks.
#[derive(Clone)]
pub struct ReflectWhitelabelVenue {
    /// Keys derived from the proxy state.
    pub keys: ReflectWhitelabelKeys,
    /// Current principal stored in the proxy state (in USDC value).
    pub principal: u64,
    /// Current integrator's commission (in USDC value).
    pub integrators_commission: u64,
    /// Fee in basis points for the integrator.
    pub fee_bps: u16,
    /// Current supply of branded tokens.
    pub branded_supply: u64,
    /// Current stablecoin balance in the vault.
    pub vault_balance: u64,
    /// Oracle price (stablecoin per USDC, scaled by precision).
    pub oracle_price: u64,
    /// Oracle precision (decimal places).
    pub oracle_precision: u8,
    /// Set of pubkeys required for state updates.
    required_state_pubkeys: HashSet<Pubkey>,
    /// Whether the venue has been fully initialized.
    found_all_pubkeys: bool,
    /// Token info for tradable tokens [stablecoin, branded].
    token_info: Vec<TokenInfo>,
}

impl FromAccount for ReflectWhitelabelVenue {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError> {
        let proxy_state = ParsedProxyState::from_bytes(&account.data).ok_or_else(|| {
            TradingVenueError::DeserializationFailed("Failed to parse ProxyState".into())
        })?;

        let stablecoin_token_program = spl_token::ID;
        let stablecoin_vault = get_associated_token_address_with_program_id(
            pubkey,
            &proxy_state.stablecoin_mint,
            &stablecoin_token_program,
        );

        let keys = ReflectWhitelabelKeys {
            proxy_state: *pubkey,
            branded_mint: proxy_state.branded_mint,
            stablecoin_mint: proxy_state.stablecoin_mint,
            authority: proxy_state.authority,
            oracle: ORACLE_ID,
            stablecoin_token_program,
            stablecoin_vault,
            bump: proxy_state.bump,
        };

        let required_state_pubkeys = HashSet::from_iter([
            keys.proxy_state,
            keys.branded_mint,
            keys.stablecoin_mint,
            keys.stablecoin_vault,
            keys.oracle,
        ]);

        Ok(Self {
            keys,
            principal: proxy_state.principal,
            integrators_commission: proxy_state.integrators_commission,
            fee_bps: proxy_state.fee,
            branded_supply: 0,
            vault_balance: 0,
            oracle_price: 0,
            oracle_precision: 6, // Default to 6 decimals
            required_state_pubkeys,
            found_all_pubkeys: false,
            token_info: Vec::new(),
        })
    }
}

#[async_trait]
impl TradingVenue for ReflectWhitelabelVenue {
    fn initialized(&self) -> bool {
        self.found_all_pubkeys
    }

    fn market_id(&self) -> Pubkey {
        self.keys.proxy_state
    }

    fn program_id(&self) -> Pubkey {
        REFLECT_PROXY_PROGRAM_ID
    }

    fn program_dependencies(&self) -> Vec<Pubkey> {
        vec![REFLECT_PROXY_PROGRAM_ID, self.keys.stablecoin_token_program]
    }

    fn protocol(&self) -> PoolProtocol {
        PoolProtocol::ReflectWhitelabel
    }

    fn tradable_mints(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(vec![self.keys.stablecoin_mint, self.keys.branded_mint])
    }

    fn decimals(&self) -> Result<Vec<i32>, TradingVenueError> {
        Ok(vec![
            self.token_info
                .first()
                .ok_or_else(|| {
                    TradingVenueError::MissingState(self.keys.stablecoin_mint.into())
                })?
                .decimals,
            self.token_info
                .get(1)
                .ok_or_else(|| {
                    TradingVenueError::MissingState(self.keys.branded_mint.into())
                })?
                .decimals,
        ])
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.token_info
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        let accounts_pubkeys = vec![
            self.keys.proxy_state,
            self.keys.branded_mint,
            self.keys.stablecoin_mint,
            self.keys.stablecoin_vault,
            self.keys.oracle,
        ];

        self.required_state_pubkeys.extend(&accounts_pubkeys);

        let accounts = cache.get_accounts(&accounts_pubkeys).await?;

        let [proxy_state_account, branded_mint_account, stablecoin_mint_account, vault_account, oracle_account]: [Option<Account>;
            5] = accounts
            .try_into()
            .map_err(|_| TradingVenueError::FailedToFetchMultipleAccountData)?;

        if let Some(ref account) = proxy_state_account {
            let proxy_state = ParsedProxyState::from_bytes(&account.data).ok_or_else(|| {
                TradingVenueError::DeserializationFailed("Failed to parse ProxyState".into())
            })?;
            self.principal = proxy_state.principal;
            self.integrators_commission = proxy_state.integrators_commission;
            self.fee_bps = proxy_state.fee;
        }

        if let Some(ref account) = branded_mint_account {
            let mint = StateWithExtensions::<Mint>::unpack(&account.data).map_err(|_| {
                TradingVenueError::DeserializationFailed("Failed to parse branded mint".into())
            })?;
            self.branded_supply = mint.base.supply;
        }

        if let Some(ref account) = stablecoin_mint_account {
            self.keys.stablecoin_token_program = account.owner;
            self.keys.stablecoin_vault = get_associated_token_address_with_program_id(
                &self.keys.proxy_state,
                &self.keys.stablecoin_mint,
                &self.keys.stablecoin_token_program,
            );
        }

        if let Some(ref account) = vault_account {
            if account.data.len() >= 72 {
                let balance_bytes: [u8; 8] = account.data[64..72]
                    .try_into()
                    .map_err(|_| TradingVenueError::DeserializationFailed("vault balance".into()))?;
                self.vault_balance = u64::from_le_bytes(balance_bytes);
            }
        }

        if let Some(ref account) = oracle_account {
            if account.data.len() >= ORACLE_LEN {
                let price_bytes: [u8; 8] = account.data[8..16]
                    .try_into()
                    .map_err(|_| TradingVenueError::DeserializationFailed("oracle price".into()))?;
                self.oracle_price = u64::from_le_bytes(price_bytes);
                self.oracle_precision = account.data[16];
            }
        }

        if let (Some(stablecoin_account), Some(branded_account)) =
            (stablecoin_mint_account, branded_mint_account)
        {
            self.token_info = vec![
                TokenInfo::new(&self.keys.stablecoin_mint, &stablecoin_account, u64::MAX)?,
                TokenInfo::new(&self.keys.branded_mint, &branded_account, u64::MAX)?,
            ];
        }

        self.found_all_pubkeys = true;

        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        if request.amount == 0 {
            return Ok(QuoteResult {
                input_mint: request.input_mint,
                output_mint: request.output_mint,
                amount: 0,
                expected_output: 0,
                not_enough_liquidity: false,
            });
        }

        let is_wrap = request.input_mint == self.keys.stablecoin_mint
            && request.output_mint == self.keys.branded_mint;
        let is_unwrap = request.input_mint == self.keys.branded_mint
            && request.output_mint == self.keys.stablecoin_mint;

        if !is_wrap && !is_unwrap {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        }

        let expected_output = if is_wrap {
            self.calculate_wrap_output(request.amount)?
        } else {
            self.calculate_unwrap_output(request.amount)?
        };

        let not_enough_liquidity = is_unwrap && expected_output > self.vault_balance;

        Ok(QuoteResult {
            input_mint: request.input_mint,
            output_mint: request.output_mint,
            amount: request.amount,
            expected_output,
            not_enough_liquidity,
        })
    }

    fn generate_swap_instruction(
        &self,
        request: QuoteRequest,
        user: Pubkey,
    ) -> Result<Instruction, TradingVenueError> {
        let is_wrap = request.input_mint == self.keys.stablecoin_mint
            && request.output_mint == self.keys.branded_mint;
        let is_unwrap = request.input_mint == self.keys.branded_mint
            && request.output_mint == self.keys.stablecoin_mint;

        if !is_wrap && !is_unwrap {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        }

        let stablecoin_user_token_account = get_associated_token_address_with_program_id(
            &user,
            &self.keys.stablecoin_mint,
            &self.keys.stablecoin_token_program,
        );

        let branded_user_token_account =
            get_associated_token_address_with_program_id(&user, &self.keys.branded_mint, &spl_token::ID);

        if is_wrap {
            self.build_wrap_instruction(
                user,
                stablecoin_user_token_account,
                branded_user_token_account,
                request.amount,
            )
        } else {
            self.build_unwrap_instruction(
                user,
                stablecoin_user_token_account,
                branded_user_token_account,
                request.amount,
            )
        }
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        if !self.found_all_pubkeys {
            return Err(TradingVenueError::NotInitialized(
                "State needs to be fully updated!".into(),
            ));
        }
        Ok(self
            .required_state_pubkeys
            .iter()
            .cloned()
            .collect::<Vec<Pubkey>>())
    }
}

impl ReflectWhitelabelVenue {
    /// Simulate the crank operation that happens on-chain before wrap/unwrap.
    /// Returns the post-crank principal value.
    fn simulate_crank(&self) -> Result<u64, TradingVenueError> {
        let vault_usdc_value = (self.vault_balance as u128)
            .checked_mul(self.oracle_price as u128)
            .and_then(|x| x.checked_div(10u128.pow(self.oracle_precision as u32)))
            .ok_or_else(|| TradingVenueError::MathError("overflow in vault value".into()))?;

        let vault_usdc_value = u64::try_from(vault_usdc_value)
            .map_err(|_| TradingVenueError::MathError("vault value overflow u64".into()))?;

        let total_principal_and_commission = self
            .principal
            .checked_add(self.integrators_commission)
            .ok_or_else(|| TradingVenueError::MathError("overflow in total".into()))?;

        let interest_generated = vault_usdc_value.saturating_sub(total_principal_and_commission);

        let generated_commission = (interest_generated as u128)
            .checked_mul(self.fee_bps as u128)
            .and_then(|x| x.checked_div(10_000))
            .ok_or_else(|| TradingVenueError::MathError("overflow in commission".into()))?;

        let generated_commission = u64::try_from(generated_commission)
            .map_err(|_| TradingVenueError::MathError("commission overflow u64".into()))?;

        let generated_principal = interest_generated
            .checked_sub(generated_commission)
            .ok_or_else(|| TradingVenueError::MathError("underflow in principal share".into()))?;

        self.principal
            .checked_add(generated_principal)
            .ok_or_else(|| TradingVenueError::MathError("overflow in new principal".into()))
    }

    /// Calculate branded tokens output for a given stablecoin input (wrap).
    fn calculate_wrap_output(&self, stablecoin_amount: u64) -> Result<u64, TradingVenueError> {
        let post_crank_principal = self.simulate_crank()?;

        let input_usdc_value = (stablecoin_amount as u128)
            .checked_mul(self.oracle_price as u128)
            .and_then(|x| x.checked_div(10u128.pow(self.oracle_precision as u32)))
            .ok_or_else(|| TradingVenueError::MathError("overflow in wrap calculation".into()))?;

        let input_usdc_value_u64 = u64::try_from(input_usdc_value)
            .map_err(|_| TradingVenueError::MathError("input USDC value overflow u64".into()))?;

        post_crank_principal
            .checked_add(input_usdc_value_u64)
            .ok_or_else(|| TradingVenueError::MathError("principal addition would overflow".into()))?;

        let supply_with_virtual = (self.branded_supply as u128)
            .checked_add(VIRTUAL_SHARES as u128)
            .ok_or_else(|| TradingVenueError::MathError("overflow in supply".into()))?;

        let principal_with_virtual = (post_crank_principal as u128)
            .checked_add(VIRTUAL_ASSETS_USDC as u128)
            .ok_or_else(|| TradingVenueError::MathError("overflow in principal".into()))?;

        let output = input_usdc_value
            .checked_mul(supply_with_virtual)
            .and_then(|x| x.checked_div(principal_with_virtual))
            .ok_or_else(|| TradingVenueError::MathError("overflow in wrap output".into()))?;

        u64::try_from(output)
            .map_err(|_| TradingVenueError::MathError("wrap output overflow u64".into()))
    }

    /// Calculate stablecoin output for a given branded token input (unwrap).
    fn calculate_unwrap_output(&self, branded_amount: u64) -> Result<u64, TradingVenueError> {
        if branded_amount > self.branded_supply {
            return Err(TradingVenueError::MathError(
                "burn amount exceeds total supply".into(),
            ));
        }

        let post_crank_principal = self.simulate_crank()?;

        let supply_with_virtual = (self.branded_supply as u128)
            .checked_add(VIRTUAL_SHARES as u128)
            .ok_or_else(|| TradingVenueError::MathError("overflow in supply".into()))?;

        let principal_with_virtual = (post_crank_principal as u128)
            .checked_add(VIRTUAL_ASSETS_USDC as u128)
            .ok_or_else(|| TradingVenueError::MathError("overflow in principal".into()))?;

        // Calculate USDC value of user's share
        let user_share_usdc_value = (branded_amount as u128)
            .checked_mul(principal_with_virtual)
            .and_then(|x| x.checked_div(supply_with_virtual))
            .ok_or_else(|| {
                TradingVenueError::MathError("overflow in unwrap share calculation".into())
            })?;

        let user_share_usdc_value_u64 = u64::try_from(user_share_usdc_value)
            .map_err(|_| TradingVenueError::MathError("user share USDC value overflow u64".into()))?;

        post_crank_principal
            .checked_sub(user_share_usdc_value_u64)
            .ok_or_else(|| TradingVenueError::MathError("principal subtraction would underflow".into()))?;

        let stablecoin_out = user_share_usdc_value
            .checked_mul(10u128.pow(self.oracle_precision as u32))
            .and_then(|x| x.checked_div(self.oracle_price as u128))
            .ok_or_else(|| {
                TradingVenueError::MathError("overflow in unwrap output calculation".into())
            })?;

        u64::try_from(stablecoin_out)
            .map_err(|_| TradingVenueError::MathError("unwrap output overflow u64".into()))
    }

    fn build_wrap_instruction(
        &self,
        user: Pubkey,
        stablecoin_user_token_account: Pubkey,
        branded_user_token_account: Pubkey,
        amount: u64,
    ) -> Result<Instruction, TradingVenueError> {
        use reflect_proxy_program_client::instructions::{Wrap, WrapInstructionArgs};

        let wrap = Wrap {
            user,
            stablecoin_user_token_account,
            branded_user_token_account,
            proxy_state: self.keys.proxy_state,
            stablecoin_proxy_state_vault: self.keys.stablecoin_vault,
            branded_mint: self.keys.branded_mint,
            oracle: self.keys.oracle,
            token_program: self.keys.stablecoin_token_program,
        };

        let args = WrapInstructionArgs {
            amount,
            min_branded_tokens: 0,
        };

        Ok(wrap.instruction(args))
    }

    fn build_unwrap_instruction(
        &self,
        user: Pubkey,
        stablecoin_user_token_account: Pubkey,
        branded_user_token_account: Pubkey,
        amount: u64,
    ) -> Result<Instruction, TradingVenueError> {
        use reflect_proxy_program_client::instructions::{Unwrap, UnwrapInstructionArgs};

        let unwrap = Unwrap {
            user,
            stablecoin_user_token_account,
            branded_user_token_account,
            proxy_state: self.keys.proxy_state,
            stablecoin_proxy_state_vault: self.keys.stablecoin_vault,
            branded_mint: self.keys.branded_mint,
            oracle: self.keys.oracle,
            token_program: self.keys.stablecoin_token_program,
        };

        let args = UnwrapInstructionArgs {
            amount,
            min_usdc_plus: 0,
        };

        Ok(unwrap.instruction(args))
    }

    pub fn set_oracle(&mut self, oracle: Pubkey) {
        self.keys.oracle = oracle;
    }

    pub fn set_oracle_price(&mut self, price: u64, precision: u8) {
        self.oracle_price = price;
        self.oracle_precision = precision;
    }
}

#[async_trait]
impl AddressLookupTableTrait for ReflectWhitelabelVenue {
    async fn get_lookup_table_keys(
        &self,
        _accounts_cache: Option<&dyn AccountsCache>,
    ) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(vec![
            self.keys.proxy_state,
            self.keys.branded_mint,
            self.keys.stablecoin_mint,
            self.keys.stablecoin_vault,
            self.keys.oracle,
            self.keys.stablecoin_token_program,
            REFLECT_PROXY_PROGRAM_ID,
        ])
    }
}
