//! Reflect Whitelabel trading venue integration for Titan.
//!
//! This module implements wrap/unwrap functionality for Reflect's whitelabel
//! stablecoin system. Users can wrap USDC+ (stablecoin) into branded tokens
//! and unwrap branded tokens back to USDC+.

mod instructions;
pub mod oracle;

use ahash::HashSet;
use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_token_2022::{extension::StateWithExtensions, state::Mint};

use oracle::{div_oracle, mul_oracle, real_price_f64, OracleLeg};

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

/// PDA seed for the `Asset` registry account (`["asset", stablecoin_mint]`).
/// The proxy program reads the oracle/feed identity from this account during
/// wrap/unwrap, so it must be supplied with the swap instruction.
const ASSET_SEED: &[u8] = b"asset";

/// Minimum slippage bound passed to on-chain wrap/unwrap. The program rejects a
/// zero minimum (it would disable slippage protection), and rejects zero output
/// separately, so `1` is the smallest accepted value. Titan enforces real
/// slippage at the route level.
const MIN_OUTPUT_TOKENS: u64 = 1;

/// Derive the `Asset` registry PDA for a given stablecoin mint.
fn derive_asset_pda(stablecoin_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[ASSET_SEED, stablecoin_mint.as_ref()],
        &REFLECT_PROXY_PROGRAM_ID,
    )
    .0
}

/// Virtual accounting offset for ERC4626-style inflation-attack mitigation.
///
/// The on-chain program computes this dynamically as `10^branded_decimals`
/// (e.g. `1_000_000` for a 6-decimal token). It is used as both the virtual
/// share offset and the virtual asset offset.
fn virtual_offset(branded_decimals: u8) -> u128 {
    10u128.pow(branded_decimals as u32)
}

/// On-chain ProxyState size: 32 + 32 + 2 + 8 + 8 + 32 + 1 = 115 bytes
const PROXY_STATE_LEN: usize = 115;

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
    /// The `Asset` registry PDA (`["asset", stablecoin_mint]`). Supplied to
    /// wrap/unwrap so the program can read the oracle/feed identity.
    pub asset: Pubkey,
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
    /// Composed oracle price for the asset's chain. `oracle_price * 10^oracle_exponent`
    /// is the real USDC value of one stablecoin (USDC+) atom.
    pub oracle_price: u128,
    /// Signed exponent accompanying [`Self::oracle_price`] (from the composed chain).
    pub oracle_exponent: i32,
    /// Decimals of the branded mint, used for the virtual accounting offset.
    pub branded_decimals: u8,
    /// Parsed oracle-chain legs from the `Asset` account (leg 0 first).
    oracle_legs: Vec<OracleLeg>,
    /// Trailing oracle-leg accounts (legs 1..) appended to wrap/unwrap after
    /// `stablecoin_mint`. Empty for single-leg assets.
    extra_oracles: Vec<Pubkey>,
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
            asset: derive_asset_pda(&proxy_state.stablecoin_mint),
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
            keys.asset,
        ]);

        Ok(Self {
            keys,
            principal: proxy_state.principal,
            integrators_commission: proxy_state.integrators_commission,
            fee_bps: proxy_state.fee,
            branded_supply: 0,
            vault_balance: 0,
            oracle_price: 0,
            oracle_exponent: 0,
            branded_decimals: 6, // Default to 6 decimals until state is loaded.
            oracle_legs: Vec::new(),
            extra_oracles: Vec::new(),
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
        // Phase 1: proxy state, mints, vault, and the Asset registry account. The
        // oracle-chain legs live inside the Asset, so their accounts are only known
        // after this fetch (phase 2 below).
        let accounts_pubkeys = vec![
            self.keys.proxy_state,
            self.keys.branded_mint,
            self.keys.stablecoin_mint,
            self.keys.stablecoin_vault,
            self.keys.asset,
        ];

        self.required_state_pubkeys.extend(&accounts_pubkeys);

        let accounts = cache.get_accounts(&accounts_pubkeys).await?;

        let [proxy_state_account, branded_mint_account, stablecoin_mint_account, vault_account, asset_account]: [Option<Account>;
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
            self.branded_decimals = mint.base.decimals;
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

        // Parse the Asset's oracle chain and resolve the composed price.
        if let Some(ref account) = asset_account {
            let legs = oracle::parse_asset_legs(&account.data)?;
            let leg_accounts = oracle::leg_oracle_accounts(&legs);

            // Leg 0 is the primary oracle the instruction passes as `oracle`; the
            // rest form the `extra_oracles` tail.
            self.keys.oracle = leg_accounts[0];
            self.extra_oracles = leg_accounts[1..].to_vec();
            self.oracle_legs = legs.clone();
            self.required_state_pubkeys.extend(leg_accounts.iter().copied());

            // Phase 2: fetch each leg's oracle account and compose the chain.
            let fetched = cache.get_accounts(&leg_accounts).await?;
            let mut resolved = Vec::with_capacity(leg_accounts.len());
            for (leg_pubkey, maybe_account) in leg_accounts.iter().zip(fetched.into_iter()) {
                let account = maybe_account
                    .ok_or_else(|| TradingVenueError::NoAccountFound((*leg_pubkey).into()))?;
                resolved.push((account.owner, account.data));
            }

            let composed = oracle::resolve_price(&legs, &resolved)?;
            self.oracle_price = composed.price;
            self.oracle_exponent = composed.exponent;
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
        let is_wrap = request.input_mint == self.keys.stablecoin_mint
            && request.output_mint == self.keys.branded_mint;
        let is_unwrap = request.input_mint == self.keys.branded_mint
            && request.output_mint == self.keys.stablecoin_mint;

        if !is_wrap && !is_unwrap {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        }

        // Wrap and unwrap are both linear in the input amount, so the marginal
        // price `f'(amount)` is constant and equal to the spot price at 0.
        let price = self.spot_price(is_wrap)?;

        // A zero-input quote produces no output, but Titan still expects the
        // venue's spot price. See [`QuoteResult::price`].
        if request.amount == 0 {
            return Ok(QuoteResult {
                input_mint: request.input_mint,
                output_mint: request.output_mint,
                amount: 0,
                expected_output: 0,
                not_enough_liquidity: false,
                price,
            });
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
            price,
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

/// Post-crank figures the wrap/unwrap share math prices against — the result of
/// the on-chain `crank` plus `redeemable_principal`.
struct CrankFigures {
    /// Principal after settling accrued interest (the on-chain post-crank principal).
    post_principal: u64,
    /// What the branded shares actually claim on the vault: `post_principal`
    /// capped at the vault value net of the unclaimed integrator commission.
    /// Equal to `post_principal` on any settled tick; smaller only when an
    /// unbooked price fall has left the vault worth less than principal +
    /// commission.
    redeemable: u64,
    /// USDC value of the vault at the current composed oracle price.
    vault_usdc_value: u64,
}

impl ReflectWhitelabelVenue {
    /// Simulate the on-chain crank and derive the figures wrap/unwrap price
    /// against. Mirrors `ProxyState::crank` followed by
    /// `ProxyState::redeemable_principal`.
    fn crank_figures(&self) -> Result<CrankFigures, TradingVenueError> {
        let vault_usdc_value = u64::try_from(mul_oracle(
            self.vault_balance as u128,
            self.oracle_price,
            self.oracle_exponent,
        )?)
        .map_err(|_| TradingVenueError::MathError("vault value overflow u64".into()))?;

        let total_principal_and_commission = self
            .principal
            .checked_add(self.integrators_commission)
            .ok_or_else(|| TradingVenueError::MathError("overflow in total".into()))?;

        let interest_generated = vault_usdc_value.saturating_sub(total_principal_and_commission);

        let generated_commission = u64::try_from(
            (interest_generated as u128)
                .checked_mul(self.fee_bps as u128)
                .and_then(|x| x.checked_div(10_000))
                .ok_or_else(|| TradingVenueError::MathError("overflow in commission".into()))?,
        )
        .map_err(|_| TradingVenueError::MathError("commission overflow u64".into()))?;

        let generated_principal = interest_generated
            .checked_sub(generated_commission)
            .ok_or_else(|| TradingVenueError::MathError("underflow in principal share".into()))?;

        let post_principal = self
            .principal
            .checked_add(generated_principal)
            .ok_or_else(|| TradingVenueError::MathError("overflow in new principal".into()))?;

        let post_commission = self
            .integrators_commission
            .checked_add(generated_commission)
            .ok_or_else(|| TradingVenueError::MathError("overflow in new commission".into()))?;

        // redeemable_principal: the recorded principal, capped at the vault value
        // once the unclaimed commission is set aside. Equals `post_principal` on a
        // settled tick (where vault value == principal + commission).
        let redeemable = post_principal.min(vault_usdc_value.saturating_sub(post_commission));

        Ok(CrankFigures {
            post_principal,
            redeemable,
            vault_usdc_value,
        })
    }

    /// Marginal price (output atoms per input atom) for a wrap or unwrap.
    ///
    /// Both `calculate_wrap_output` and `calculate_unwrap_output` are linear in
    /// the input amount — the redeemable principal, branded supply, and oracle
    /// price do not depend on the trade size — so `f'(amount)` is constant and
    /// equals the venue's spot price at `amount == 0`. This satisfies Titan's
    /// pricing invariants: the output curve is a positive-slope line, hence
    /// monotone, concave (constant price), and consistent with the realized
    /// average rate.
    fn spot_price(&self, is_wrap: bool) -> Result<f64, TradingVenueError> {
        let figures = self.crank_figures()?;

        let voff = virtual_offset(self.branded_decimals) as f64;
        let supply_with_virtual = self.branded_supply as f64 + voff;
        // The share math prices against the redeemable principal, not the raw
        // post-crank principal (they differ only in an undercollateralized vault).
        let redeemable_with_virtual = figures.redeemable as f64 + voff;
        // Real USDC value of one stablecoin (USDC+) atom = price * 10^exponent.
        let real_price = real_price_f64(self.oracle_price, self.oracle_exponent);

        if real_price <= 0.0 || redeemable_with_virtual <= 0.0 {
            return Err(TradingVenueError::MathError(
                "spot price is undefined for current venue state".into(),
            ));
        }

        let price = if is_wrap {
            // branded out per stablecoin in
            real_price * (supply_with_virtual / redeemable_with_virtual)
        } else {
            // stablecoin out per branded in
            (redeemable_with_virtual / supply_with_virtual) / real_price
        };

        Ok(price)
    }

    /// Calculate branded tokens output for a given stablecoin input (wrap).
    fn calculate_wrap_output(&self, stablecoin_amount: u64) -> Result<u64, TradingVenueError> {
        let figures = self.crank_figures()?;

        // On-chain wrap refuses an undercollateralized vault — the mint and the
        // burn must share one basis — and a vault whose shares claim nothing.
        if figures.vault_usdc_value < figures.post_principal
            || (figures.redeemable == 0 && self.branded_supply > 0)
        {
            return Err(TradingVenueError::InactivePoolError(
                self.keys.proxy_state,
                PoolProtocol::ReflectWhitelabel,
            ));
        }

        // Match the on-chain u64 truncation of the USDC value before the share math.
        let input_usdc_value = u64::try_from(mul_oracle(
            stablecoin_amount as u128,
            self.oracle_price,
            self.oracle_exponent,
        )?)
        .map_err(|_| TradingVenueError::MathError("input USDC value overflow u64".into()))?;

        let supply_with_virtual = (self.branded_supply as u128)
            .checked_add(virtual_offset(self.branded_decimals))
            .ok_or_else(|| TradingVenueError::MathError("overflow in supply".into()))?;

        // Price against the redeemable principal, matching on-chain.
        let redeemable_with_virtual = (figures.redeemable as u128)
            .checked_add(virtual_offset(self.branded_decimals))
            .ok_or_else(|| TradingVenueError::MathError("overflow in principal".into()))?;

        let output = (input_usdc_value as u128)
            .checked_mul(supply_with_virtual)
            .and_then(|x| x.checked_div(redeemable_with_virtual))
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

        let figures = self.crank_figures()?;

        // On-chain unwrap refuses a vault whose shares claim nothing.
        if figures.redeemable == 0 && self.branded_supply > 0 {
            return Err(TradingVenueError::InactivePoolError(
                self.keys.proxy_state,
                PoolProtocol::ReflectWhitelabel,
            ));
        }

        let supply_with_virtual = (self.branded_supply as u128)
            .checked_add(virtual_offset(self.branded_decimals))
            .ok_or_else(|| TradingVenueError::MathError("overflow in supply".into()))?;

        // The payout is priced against the redeemable principal, so an exiter
        // cannot draw on the integrator commission or uncovered collateral.
        let redeemable_with_virtual = (figures.redeemable as u128)
            .checked_add(virtual_offset(self.branded_decimals))
            .ok_or_else(|| TradingVenueError::MathError("overflow in principal".into()))?;

        // Match the on-chain u64 truncation of the share value before converting.
        let user_share_usdc_value = u64::try_from(
            (branded_amount as u128)
                .checked_mul(redeemable_with_virtual)
                .and_then(|x| x.checked_div(supply_with_virtual))
                .ok_or_else(|| {
                    TradingVenueError::MathError("overflow in unwrap share calculation".into())
                })?,
        )
        .map_err(|_| TradingVenueError::MathError("user share USDC value overflow u64".into()))?;

        let stablecoin_out = div_oracle(
            user_share_usdc_value as u128,
            self.oracle_price,
            self.oracle_exponent,
        )?;

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
        use instructions::{Wrap, WrapInstructionArgs};

        let wrap = Wrap {
            user,
            stablecoin_user_token_account,
            branded_user_token_account,
            proxy_state: self.keys.proxy_state,
            stablecoin_proxy_state_vault: self.keys.stablecoin_vault,
            branded_mint: self.keys.branded_mint,
            oracle: self.keys.oracle,
            asset: self.keys.asset,
            token_program: self.keys.stablecoin_token_program,
            stablecoin_mint: self.keys.stablecoin_mint,
            extra_oracles: self.extra_oracles.clone(),
        };

        let args = WrapInstructionArgs {
            amount,
            min_branded_tokens: MIN_OUTPUT_TOKENS,
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
        use instructions::{Unwrap, UnwrapInstructionArgs};

        let unwrap = Unwrap {
            user,
            stablecoin_user_token_account,
            branded_user_token_account,
            proxy_state: self.keys.proxy_state,
            stablecoin_proxy_state_vault: self.keys.stablecoin_vault,
            branded_mint: self.keys.branded_mint,
            oracle: self.keys.oracle,
            asset: self.keys.asset,
            token_program: self.keys.stablecoin_token_program,
            stablecoin_mint: self.keys.stablecoin_mint,
            extra_oracles: self.extra_oracles.clone(),
        };

        let args = UnwrapInstructionArgs {
            amount,
            min_usdc_plus: MIN_OUTPUT_TOKENS,
        };

        Ok(unwrap.instruction(args))
    }

    pub fn set_oracle(&mut self, oracle: Pubkey) {
        self.keys.oracle = oracle;
    }

    /// Override the composed oracle price. `price * 10^exponent` is the real USDC
    /// value of one stablecoin (USDC+) atom. Primarily for tests.
    pub fn set_oracle_price(&mut self, price: u128, exponent: i32) {
        self.oracle_price = price;
        self.oracle_exponent = exponent;
    }

    /// Override the branded-mint decimals used for the virtual accounting offset.
    /// Primarily for tests.
    pub fn set_branded_decimals(&mut self, decimals: u8) {
        self.branded_decimals = decimals;
    }
}

#[async_trait]
impl AddressLookupTableTrait for ReflectWhitelabelVenue {
    async fn get_lookup_table_keys(
        &self,
        _accounts_cache: Option<&dyn AccountsCache>,
    ) -> Result<Vec<Pubkey>, TradingVenueError> {
        let mut keys = vec![
            self.keys.proxy_state,
            self.keys.branded_mint,
            self.keys.stablecoin_mint,
            self.keys.stablecoin_vault,
            self.keys.oracle,
            self.keys.asset,
            self.keys.stablecoin_token_program,
            REFLECT_PROXY_PROGRAM_ID,
        ];
        keys.extend(self.extra_oracles.iter().copied());
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::instructions::{Unwrap, UnwrapInstructionArgs, Wrap, WrapInstructionArgs};
    use super::*;

    fn pk(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    /// The deployed program (`main`) destructures wrap/unwrap accounts as a fixed
    /// 10-slot array with `asset` at index 7 and `stablecoin_mint` at index 9,
    /// followed by an optional oracle-chain tail. Lock that shape in.
    #[test]
    fn wrap_instruction_matches_onchain_account_layout() {
        let wrap = Wrap {
            user: pk(1),
            stablecoin_user_token_account: pk(2),
            branded_user_token_account: pk(3),
            proxy_state: pk(4),
            stablecoin_proxy_state_vault: pk(5),
            branded_mint: pk(6),
            oracle: pk(7),
            asset: pk(8),
            token_program: pk(9),
            stablecoin_mint: pk(10),
            extra_oracles: Vec::new(),
        };

        let ix = wrap.instruction(WrapInstructionArgs {
            amount: 1_000,
            min_branded_tokens: MIN_OUTPUT_TOKENS,
        });

        assert_eq!(ix.program_id, REFLECT_PROXY_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 10, "single-leg wrap must pass 10 accounts");

        // Order and mutability that the program's `WrapAccounts` destructure expects.
        assert_eq!(ix.accounts[0].pubkey, pk(1));
        assert!(ix.accounts[0].is_signer && !ix.accounts[0].is_writable);
        assert!(ix.accounts[3].is_writable, "proxy_state must be mutable");
        assert!(ix.accounts[5].is_writable, "branded_mint must be mutable");
        assert_eq!(ix.accounts[6].pubkey, pk(7)); // oracle
        assert_eq!(ix.accounts[7].pubkey, pk(8)); // asset
        assert!(!ix.accounts[7].is_writable);
        assert_eq!(ix.accounts[8].pubkey, pk(9)); // token_program
        assert_eq!(ix.accounts[9].pubkey, pk(10)); // stablecoin_mint

        // discriminator (1) + amount (8) + min (8)
        assert_eq!(ix.data[0], 1);
        assert_eq!(ix.data.len(), 17);
        let min = u64::from_le_bytes(ix.data[9..17].try_into().unwrap());
        assert!(min > 0, "on-chain rejects a zero minimum");
    }

    #[test]
    fn unwrap_instruction_matches_onchain_account_layout() {
        let unwrap = Unwrap {
            user: pk(1),
            stablecoin_user_token_account: pk(2),
            branded_user_token_account: pk(3),
            proxy_state: pk(4),
            stablecoin_proxy_state_vault: pk(5),
            branded_mint: pk(6),
            oracle: pk(7),
            asset: pk(8),
            token_program: pk(9),
            stablecoin_mint: pk(10),
            extra_oracles: Vec::new(),
        };

        let ix = unwrap.instruction(UnwrapInstructionArgs {
            amount: 1_000,
            min_usdc_plus: MIN_OUTPUT_TOKENS,
        });

        assert_eq!(ix.accounts.len(), 10);
        assert_eq!(ix.accounts[7].pubkey, pk(8)); // asset
        assert_eq!(ix.accounts[9].pubkey, pk(10)); // stablecoin_mint
        assert_eq!(ix.data[0], 2);
        let min = u64::from_le_bytes(ix.data[9..17].try_into().unwrap());
        assert!(min > 0);
    }

    /// Multi-leg (Chainlink-stacked) assets append their extra oracle accounts
    /// after `stablecoin_mint`, matching the program's `extra_oracles @ ..` tail.
    #[test]
    fn extra_oracle_legs_are_appended_in_order() {
        let wrap = Wrap {
            user: pk(1),
            stablecoin_user_token_account: pk(2),
            branded_user_token_account: pk(3),
            proxy_state: pk(4),
            stablecoin_proxy_state_vault: pk(5),
            branded_mint: pk(6),
            oracle: pk(7),
            asset: pk(8),
            token_program: pk(9),
            stablecoin_mint: pk(10),
            extra_oracles: vec![pk(11), pk(12)],
        };

        let ix = wrap.instruction(WrapInstructionArgs {
            amount: 1,
            min_branded_tokens: 1,
        });

        assert_eq!(ix.accounts.len(), 12);
        assert_eq!(ix.accounts[10].pubkey, pk(11));
        assert_eq!(ix.accounts[11].pubkey, pk(12));
    }

    #[test]
    fn asset_pda_is_derived_off_the_stablecoin_mint() {
        let mint = pk(42);
        let asset = derive_asset_pda(&mint);
        // Deterministic and program-owned derivation (does not equal the mint).
        assert_eq!(asset, derive_asset_pda(&mint));
        assert_ne!(asset, mint);
    }

    /// Build a venue in a fixed state for exercising the quote math directly.
    /// Oracle is a 1.0 USDC/USDC+ price (`1e6` at exponent `-6`); branded token
    /// has 6 decimals (virtual offset `1e6`).
    fn quote_venue(
        principal: u64,
        integrators_commission: u64,
        fee_bps: u16,
        branded_supply: u64,
        vault_balance: u64,
    ) -> ReflectWhitelabelVenue {
        ReflectWhitelabelVenue {
            keys: ReflectWhitelabelKeys {
                proxy_state: pk(4),
                branded_mint: pk(6),
                stablecoin_mint: pk(20),
                authority: pk(21),
                asset: pk(8),
                oracle: pk(7),
                stablecoin_token_program: spl_token::ID,
                stablecoin_vault: pk(5),
                bump: 255,
            },
            principal,
            integrators_commission,
            fee_bps,
            branded_supply,
            vault_balance,
            oracle_price: 1_000_000,
            oracle_exponent: -6,
            branded_decimals: 6,
            oracle_legs: Vec::new(),
            extra_oracles: Vec::new(),
            required_state_pubkeys: ahash::HashSet::default(),
            found_all_pubkeys: true,
            token_info: Vec::new(),
        }
    }

    #[test]
    fn redeemable_principal_leaves_healthy_quotes_unchanged() {
        // Settled tick: vault value == principal + commission, so redeemable ==
        // principal and the quotes match the pre-audit (principal-based) math.
        let v = quote_venue(100_000_000, 0, 1_000, 100_000_000, 100_000_000);
        assert_eq!(v.calculate_wrap_output(10_000_000).unwrap(), 10_000_000);
        assert_eq!(v.calculate_unwrap_output(10_000_000).unwrap(), 10_000_000);
    }

    #[test]
    fn redeemable_principal_caps_payout_when_undercollateralized() {
        // Unbooked fall: vault (90) < principal (100) + commission (10), so
        // redeemable = min(100, 90 - 10) = 80, below principal.
        let v = quote_venue(100_000_000, 10_000_000, 1_000, 100_000_000, 90_000_000);

        // Unwrap now prices against redeemable (80), not principal (100):
        // 10e6 * (80e6 + 1e6) / (100e6 + 1e6) = 8_019_801, down from 10e6.
        assert_eq!(v.calculate_unwrap_output(10_000_000).unwrap(), 8_019_801);

        // Wrap into a vault worth less than principal is refused, matching the
        // on-chain `VaultUndercollateralized` guard.
        assert!(matches!(
            v.calculate_wrap_output(10_000_000),
            Err(TradingVenueError::InactivePoolError(_, PoolProtocol::ReflectWhitelabel))
        ));
    }
}
