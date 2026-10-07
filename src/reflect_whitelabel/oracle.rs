//! Off-chain port of the Reflect proxy program's oracle-chaining logic
//! (`feat/oracle-stacking`).
//!
//! On that branch an [`Asset`] prices its stablecoin through a *chain* of 1..=3
//! oracle legs, each of a different provider (Doppler, Pyth, Chainlink Data
//! Feeds, or Chainlink Data Streams). Wrap/unwrap resolve the chain into a
//! single `(price, exponent)` and feed it to the same virtual-offset share math
//! the single-oracle version used.
//!
//! To quote such an asset correctly the adapter must reproduce that resolution
//! exactly. This module mirrors `program/src/helpers.rs` and the leg layout in
//! `program/src/state.rs`:
//!
//! - [`OraclePrice`] — the normalized `(price, exponent, confidence)` a leg reads.
//! - the per-provider readers ([`read_doppler`], [`read_pyth`],
//!   [`read_chainlink_push`], [`read_stream_price`], [`read_stream_rate`]).
//! - [`compose_oracle_legs`] — the exact product/inverse composition at
//!   `COMPOSITION_SCALE`.
//! - [`parse_asset_legs`] / [`resolve_price`] — read the Asset and walk its legs.
//!
//! Scope: this is a *quoter*, so it reproduces the numeric price the program
//! will use, not its liveness policy. Staleness and confidence-width checks
//! (which need the on-chain clock and would only cause the swap to revert, not
//! change the quoted amount) are intentionally not enforced here; the price and
//! exponent are extracted exactly.

use solana_pubkey::Pubkey;

use crate::trading_venue::error::TradingVenueError;

// ─── Composition constants (constants.rs) ────────────────────────────────────

/// Internal scale used while composing multiple oracle legs (10^18).
pub const COMPOSITION_SCALE: u128 = 1_000_000_000_000_000_000;
/// Exponent that accompanies a composed price held at [`COMPOSITION_SCALE`].
pub const COMPOSITION_EXPONENT: i32 = -18;
/// Maximum number of oracle legs a single Asset may chain.
pub const MAX_ORACLE_LEGS: usize = 3;

// ─── Asset layout (state.rs) ─────────────────────────────────────────────────

/// Current on-chain `Asset` layout version (`feat/oracle-stacking`).
pub const ASSET_VERSION: u8 = 2;
/// Total serialized length of the `Asset` account.
pub const ASSET_LEN: usize = 259;
const ASSET_LEG_COUNT_OFFSET: usize = 34;
const ASSET_VERSION_OFFSET: usize = 35;
const ASSET_LEGS_OFFSET: usize = 40;
/// Serialized length of one `OracleLegSlot`.
const LEG_LEN: usize = 73;

// ─── Oracle program owners (constants.rs) ────────────────────────────────────
//
// These identify the program that *owns* a leg's oracle account, used to pick a
// reader for an untagged (`Unset`) leg. They must match the deployed program's
// configured IDs; update for a prod/staging deployment if those differ.

/// Doppler oracle program.
pub const DOPPLER_ORACLE_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x05, 0xbe, 0xb9, 0xd8, 0x8c, 0xb5, 0xc1, 0xa2, 0x1e, 0x48, 0xe9, 0x94, 0x3b, 0x25, 0x84, 0xd6,
    0xe9, 0x30, 0x52, 0x66, 0x2a, 0x83, 0x99, 0x72, 0x3f, 0xcd, 0xac, 0x29, 0x36, 0xe1, 0x3b, 0x93,
]);
/// Pyth Solana Receiver v2 program (owns `PriceUpdateV2` accounts).
pub const PYTH_ORACLE_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x0c, 0xb7, 0xfa, 0xbb, 0x52, 0xf7, 0xa6, 0x48, 0xbb, 0x5b, 0x31, 0x7d, 0x9a, 0x01, 0x8b, 0x90,
    0x57, 0xcb, 0x02, 0x47, 0x74, 0xfa, 0xfe, 0x01, 0xe6, 0xc4, 0xdf, 0x98, 0xcc, 0x38, 0x58, 0x81,
]);
/// Chainlink OCR2 store program (owns Data Feeds `Transmissions` accounts).
pub const CHAINLINK_ORACLE_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0xf1, 0x4b, 0xf6, 0x5a, 0xd5, 0x6b, 0xd2, 0xba, 0x71, 0x5e, 0x45, 0x74, 0x2c, 0x23, 0x1f, 0x27,
    0xd6, 0x36, 0x21, 0xcf, 0x5b, 0x77, 0x8f, 0x37, 0xc1, 0xa2, 0x48, 0x95, 0x1d, 0x17, 0x56, 0x02,
]);

// ─── Provider tags (helpers.rs `OracleProvider`) ─────────────────────────────

/// Per-leg provider tag. `Unset` dispatches on the account owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleProvider {
    Unset,
    Doppler,
    Pyth,
    ChainlinkPush,
    ChainlinkStreamFeed,
    ChainlinkStreamRate,
}

impl OracleProvider {
    fn from_u8(v: u8) -> Result<Self, TradingVenueError> {
        Ok(match v {
            0 => Self::Unset,
            1 => Self::Doppler,
            2 => Self::Pyth,
            3 => Self::ChainlinkPush,
            4 => Self::ChainlinkStreamFeed,
            5 => Self::ChainlinkStreamRate,
            _ => return Err(oracle_err("invalid oracle provider tag")),
        })
    }
}

/// One configured oracle leg parsed from the `Asset` account.
#[derive(Clone, Copy, Debug)]
pub struct OracleLeg {
    /// Account holding the price for this leg.
    pub oracle: Pubkey,
    /// Expected feed identity (Pyth `feed_id` / Data Streams feed id; the account
    /// address itself for Doppler and Chainlink Data Feeds).
    pub feed_id: [u8; 32],
    /// Divide by this leg's price instead of multiplying.
    pub inverse: bool,
    /// Provider tag.
    pub provider: OracleProvider,
    /// Declared decimal scale for a Data Streams leg (zero elsewhere).
    pub decimals: u8,
}

/// Normalized reading of a single oracle. `price * 10^exponent` is the real price.
#[derive(Clone, Copy, Debug)]
pub struct OraclePrice {
    pub price: u128,
    pub exponent: i32,
    /// Confidence interval in basis points, if the provider publishes one.
    pub confidence_bps: Option<u16>,
}

fn oracle_err(msg: &'static str) -> TradingVenueError {
    TradingVenueError::MathError(msg.into())
}

// ─── Fixed-point scaling (helpers.rs) ────────────────────────────────────────

/// Scale `value` by `10^shift` (signed; negative divides, rounding toward zero).
fn scale_pow10(value: u128, shift: i32) -> Result<u128, TradingVenueError> {
    if shift >= 0 {
        let factor = 10u128
            .checked_pow(shift as u32)
            .ok_or_else(|| oracle_err("oracle scale overflow"))?;
        value
            .checked_mul(factor)
            .ok_or_else(|| oracle_err("oracle scale overflow"))
    } else {
        let factor = 10u128
            .checked_pow(shift.unsigned_abs())
            .ok_or_else(|| oracle_err("oracle scale overflow"))?;
        value
            .checked_div(factor)
            .ok_or_else(|| oracle_err("oracle scale overflow"))
    }
}

/// `floor(a * b / d)` over a 256-bit intermediate — a faithful port of the
/// program's `mul_div_floor`, so composition rounds identically.
fn mul_div_floor(a: u128, b: u128, d: u128) -> Result<u128, TradingVenueError> {
    if d == 0 {
        return Err(oracle_err("oracle divide by zero"));
    }

    const MASK: u128 = u64::MAX as u128;
    let (a_hi, a_lo) = (a >> 64, a & MASK);
    let (b_hi, b_lo) = (b >> 64, b & MASK);

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    let mid = (ll >> 64) + (lh & MASK) + (hl & MASK);
    let lo = (ll & MASK) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);

    // Fast path: the odd part of `d` fits a u64.
    let twos = d.trailing_zeros();
    let odd = d >> twos;
    if let Ok(divisor) = u64::try_from(odd) {
        let (mut s_hi, mut s_lo) = (hi, lo);
        if twos > 0 {
            s_lo = (s_lo >> twos) | (s_hi << (128 - twos));
            s_hi >>= twos;
        }

        let divisor = divisor as u128;
        let limbs = [
            (s_hi >> 64) as u64,
            s_hi as u64,
            (s_lo >> 64) as u64,
            s_lo as u64,
        ];
        let mut remainder: u128 = 0;
        let mut quotient = [0u64; 4];

        for (index, limb) in limbs.iter().enumerate() {
            let current = (remainder << 64) | *limb as u128;
            quotient[index] = (current / divisor) as u64;
            remainder = current % divisor;
        }

        if quotient[0] != 0 || quotient[1] != 0 {
            return Err(oracle_err("oracle math overflow"));
        }

        return Ok(((quotient[2] as u128) << 64) | quotient[3] as u128);
    }

    // General path: binary long division of the 256-bit value by `d`.
    if hi >= d {
        return Err(oracle_err("oracle math overflow"));
    }

    let mut remainder = hi;
    let mut quotient: u128 = 0;

    for bit in (0..128).rev() {
        let overflowed = remainder >> 127 == 1;
        remainder = (remainder << 1) | ((lo >> bit) & 1);

        if overflowed || remainder >= d {
            remainder = remainder.wrapping_sub(d);
            quotient |= 1u128 << bit;
        }
    }

    Ok(quotient)
}

/// Fold a chain of `(price, inverse)` legs into one composed price. Exact port
/// of `compose_oracle_legs`.
pub fn compose_oracle_legs(
    legs: &[(OraclePrice, bool)],
) -> Result<OraclePrice, TradingVenueError> {
    if legs.is_empty() {
        return Err(oracle_err("empty oracle chain"));
    }

    // A single straight leg passes through without scaling or rounding.
    if legs.len() == 1 && !legs[0].1 {
        if legs[0].0.price == 0 {
            return Err(oracle_err("oracle price is zero"));
        }
        return Ok(legs[0].0);
    }

    let mut acc: u128 = COMPOSITION_SCALE;
    let mut confidence_bps: Option<u16> = None;

    for (leg, inverse) in legs.iter() {
        let shift = leg
            .exponent
            .checked_add(-COMPOSITION_EXPONENT)
            .ok_or_else(|| oracle_err("oracle exponent overflow"))?;
        let scaled = scale_pow10(leg.price, shift)?;

        if scaled == 0 {
            return Err(oracle_err("oracle price is zero"));
        }

        acc = if *inverse {
            mul_div_floor(acc, COMPOSITION_SCALE, scaled)?
        } else {
            mul_div_floor(acc, scaled, COMPOSITION_SCALE)?
        };

        if let Some(leg_bps) = leg.confidence_bps {
            confidence_bps = Some(confidence_bps.unwrap_or(0).saturating_add(leg_bps));
        }
    }

    if acc == 0 {
        return Err(oracle_err("oracle price is zero"));
    }

    Ok(OraclePrice {
        price: acc,
        exponent: COMPOSITION_EXPONENT,
        confidence_bps,
    })
}

// ─── Per-provider readers (helpers.rs) ───────────────────────────────────────

/// Doppler: 17 bytes `[slot u64][price u64][precision u8]`.
const DOPPLER_LEN: usize = 17;

pub fn read_doppler(data: &[u8]) -> Result<OraclePrice, TradingVenueError> {
    if data.len() != DOPPLER_LEN {
        return Err(oracle_err("doppler oracle wrong length"));
    }
    let price = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let precision = data[16];
    if price == 0 {
        return Err(oracle_err("doppler oracle price is zero"));
    }
    Ok(OraclePrice {
        price: price as u128,
        exponent: -(precision as i32),
        confidence_bps: None,
    })
}

// Pyth PriceUpdateV2 (after the 8-byte anchor discriminator + 32-byte authority
// + variable VerificationLevel enum).
const PYTH_ANCHOR_DISC: usize = 8;
const PYTH_WRITE_AUTHORITY_LEN: usize = 32;
const PFM_FEED_ID: usize = 0;
const PFM_PRICE: usize = 32;
const PFM_CONF: usize = 40;
const PFM_EXPONENT: usize = 48;
const PFM_LEN: usize = 84;
const PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];
const BPS_DENOMINATOR: u128 = 10_000;

pub fn read_pyth(
    data: &[u8],
    expected_feed_id: &[u8; 32],
) -> Result<OraclePrice, TradingVenueError> {
    let enum_offset = PYTH_ANCHOR_DISC + PYTH_WRITE_AUTHORITY_LEN;
    if data.len() <= enum_offset {
        return Err(oracle_err("pyth account too short"));
    }
    if data[0..PYTH_ANCHOR_DISC] != PRICE_UPDATE_V2_DISCRIMINATOR {
        return Err(oracle_err("not a PriceUpdateV2 account"));
    }

    // VerificationLevel: 0 => Partial { num_signatures: u8 } (2 bytes), 1 => Full (1).
    let enum_len = match data[enum_offset] {
        0 => 2,
        1 => 1,
        _ => return Err(oracle_err("invalid pyth verification level")),
    };

    let pfm_base = enum_offset + enum_len;
    let min_len = pfm_base + PFM_LEN + 8;
    if data.len() < min_len {
        return Err(oracle_err("pyth account too short"));
    }

    if data[pfm_base + PFM_FEED_ID..pfm_base + PFM_FEED_ID + 32] != expected_feed_id[..] {
        return Err(oracle_err("pyth feed id mismatch"));
    }

    let price_raw = i64::from_le_bytes(
        data[pfm_base + PFM_PRICE..pfm_base + PFM_PRICE + 8]
            .try_into()
            .unwrap(),
    );
    let conf = u64::from_le_bytes(
        data[pfm_base + PFM_CONF..pfm_base + PFM_CONF + 8]
            .try_into()
            .unwrap(),
    );
    let exponent = i32::from_le_bytes(
        data[pfm_base + PFM_EXPONENT..pfm_base + PFM_EXPONENT + 4]
            .try_into()
            .unwrap(),
    );

    if price_raw <= 0 {
        return Err(oracle_err("pyth price not positive"));
    }

    // Same rounding as the program: ceil(conf * 10_000 / price).
    let price_u128 = (price_raw as u128).max(1);
    let confidence_bps =
        u16::try_from(((conf as u128) * BPS_DENOMINATOR + price_u128 - 1) / price_u128)
            .unwrap_or(u16::MAX);

    Ok(OraclePrice {
        price: price_raw as u128,
        exponent,
        confidence_bps: Some(confidence_bps),
    })
}

// Chainlink Data Feeds (`Transmissions`).
const CL_DISCRIMINATOR: [u8; 8] = [96, 179, 69, 66, 128, 129, 73, 117];
const CL_HEADER_END: usize = 8 + 192;
const CL_DECIMALS: usize = 138;
const CL_ENTRY_LEN: usize = 48;
const CL_T_ANSWER: usize = CL_HEADER_END + 16;

pub fn read_chainlink_push(data: &[u8]) -> Result<OraclePrice, TradingVenueError> {
    if data.len() < CL_HEADER_END + CL_ENTRY_LEN {
        return Err(oracle_err("chainlink feed too short"));
    }
    if data[0..8] != CL_DISCRIMINATOR {
        return Err(oracle_err("not a chainlink Transmissions account"));
    }

    let decimals = data[CL_DECIMALS];
    let answer = i128::from_le_bytes(data[CL_T_ANSWER..CL_T_ANSWER + 16].try_into().unwrap());
    if answer <= 0 {
        return Err(oracle_err("chainlink answer not positive"));
    }

    Ok(OraclePrice {
        price: answer as u128,
        exponent: -(decimals as i32),
        confidence_bps: None,
    })
}

// Chainlink Data Streams (`StreamPrice`, written by `post_oracle_price`).
const STREAM_PRICE_LEN: usize = 75;
const STREAM_PRICE_DISCRIMINATOR: [u8; 8] = [25, 189, 187, 86, 251, 249, 123, 93];
const SP_FEED_ID: usize = 8;
const SP_PRICE: usize = 40;
const SP_EXPONENT: usize = 56;
const SP_CONFIDENCE_BPS: usize = 60;

fn read_stream_common(
    data: &[u8],
    expected_feed_id: &[u8; 32],
    decimals: u8,
) -> Result<(u128, i32, u16), TradingVenueError> {
    if data.len() != STREAM_PRICE_LEN {
        return Err(oracle_err("stream price wrong length"));
    }
    if data[0..8] != STREAM_PRICE_DISCRIMINATOR {
        return Err(oracle_err("not a StreamPrice account"));
    }
    if data[SP_FEED_ID..SP_FEED_ID + 32] != expected_feed_id[..] {
        return Err(oracle_err("stream feed id mismatch"));
    }

    let price = u128::from_le_bytes(data[SP_PRICE..SP_PRICE + 16].try_into().unwrap());
    let exponent = i32::from_le_bytes(data[SP_EXPONENT..SP_EXPONENT + 4].try_into().unwrap());
    let confidence_bps =
        u16::from_le_bytes(data[SP_CONFIDENCE_BPS..SP_CONFIDENCE_BPS + 2].try_into().unwrap());

    // The stored exponent must match the leg's declared scale (`-decimals`),
    // otherwise a leg would inherit another asset's scale.
    if exponent != -(decimals as i32) {
        return Err(oracle_err("stream price scale mismatch"));
    }
    if price == 0 {
        return Err(oracle_err("stream price is zero"));
    }

    Ok((price, exponent, confidence_bps))
}

/// Data Streams v3 price schema — carries a confidence measure.
pub fn read_stream_price(
    data: &[u8],
    expected_feed_id: &[u8; 32],
    decimals: u8,
) -> Result<OraclePrice, TradingVenueError> {
    let (price, exponent, confidence_bps) = read_stream_common(data, expected_feed_id, decimals)?;
    Ok(OraclePrice {
        price,
        exponent,
        confidence_bps: Some(confidence_bps),
    })
}

/// Data Streams v7 redemption-rate schema — publishes no confidence.
pub fn read_stream_rate(
    data: &[u8],
    expected_feed_id: &[u8; 32],
    decimals: u8,
) -> Result<OraclePrice, TradingVenueError> {
    let (price, exponent, _confidence_bps) = read_stream_common(data, expected_feed_id, decimals)?;
    Ok(OraclePrice {
        price,
        exponent,
        confidence_bps: None,
    })
}

/// Read one leg's oracle account into an [`OraclePrice`], dispatching by tag and
/// (for `Unset`) by account owner — mirroring `load_oracle_price[_tagged]`.
pub fn read_leg_price(
    leg: &OracleLeg,
    owner: &Pubkey,
    data: &[u8],
) -> Result<OraclePrice, TradingVenueError> {
    match leg.provider {
        OracleProvider::Doppler => read_doppler(data),
        OracleProvider::Pyth => read_pyth(data, &leg.feed_id),
        OracleProvider::ChainlinkPush => read_chainlink_push(data),
        OracleProvider::ChainlinkStreamFeed => read_stream_price(data, &leg.feed_id, leg.decimals),
        OracleProvider::ChainlinkStreamRate => read_stream_rate(data, &leg.feed_id, leg.decimals),
        OracleProvider::Unset => {
            if *owner == DOPPLER_ORACLE_PROGRAM_ID || data.len() == DOPPLER_LEN {
                read_doppler(data)
            } else if *owner == PYTH_ORACLE_PROGRAM_ID {
                read_pyth(data, &leg.feed_id)
            } else if *owner == CHAINLINK_ORACLE_PROGRAM_ID {
                read_chainlink_push(data)
            } else if *owner == super::REFLECT_PROXY_PROGRAM_ID {
                read_stream_price(data, &leg.feed_id, leg.decimals)
            } else {
                Err(oracle_err("unsupported oracle account owner"))
            }
        }
    }
}

// ─── Asset parsing + chain resolution (state.rs) ─────────────────────────────

/// Parse the configured oracle legs out of an `Asset` account.
///
/// Accepts only the current [`ASSET_VERSION`] layout; a legacy (pre-legs) Asset
/// must be migrated on-chain first.
pub fn parse_asset_legs(data: &[u8]) -> Result<Vec<OracleLeg>, TradingVenueError> {
    if data.len() != ASSET_LEN {
        return Err(oracle_err("asset account wrong length"));
    }
    if data[ASSET_VERSION_OFFSET] != ASSET_VERSION {
        return Err(oracle_err("unsupported asset version"));
    }

    let leg_count = data[ASSET_LEG_COUNT_OFFSET] as usize;
    if leg_count == 0 || leg_count > MAX_ORACLE_LEGS {
        return Err(oracle_err("invalid asset leg count"));
    }

    let mut legs = Vec::with_capacity(leg_count);
    for i in 0..leg_count {
        let base = ASSET_LEGS_OFFSET + i * LEG_LEN;
        let slot = &data[base..base + LEG_LEN];
        legs.push(OracleLeg {
            oracle: Pubkey::try_from(&slot[0..32])
                .map_err(|_| oracle_err("bad leg oracle pubkey"))?,
            feed_id: slot[32..64].try_into().unwrap(),
            inverse: slot[68] == 1,
            provider: OracleProvider::from_u8(slot[69])?,
            decimals: slot[72],
        });
    }

    Ok(legs)
}

/// The oracle account a leg reads from, in the order the wrap/unwrap instruction
/// passes them: leg 0 is the primary `oracle`, the rest are the `extra_oracles`
/// tail.
pub fn leg_oracle_accounts(legs: &[OracleLeg]) -> Vec<Pubkey> {
    legs.iter().map(|leg| leg.oracle).collect()
}

/// Resolve the chain into one composed price.
///
/// `accounts[i]` must be the `(owner, data)` of `legs[i].oracle`, in leg order.
pub fn resolve_price(
    legs: &[OracleLeg],
    accounts: &[(Pubkey, Vec<u8>)],
) -> Result<OraclePrice, TradingVenueError> {
    if legs.is_empty() || accounts.len() < legs.len() {
        return Err(oracle_err("missing oracle leg account"));
    }

    let mut composed_input = Vec::with_capacity(legs.len());
    for (leg, (owner, data)) in legs.iter().zip(accounts.iter()) {
        let price = read_leg_price(leg, owner, data)?;
        composed_input.push((price, leg.inverse));
    }

    compose_oracle_legs(&composed_input)
}

// ─── Quote-math scaling (state.rs mul_oracle / div_oracle) ────────────────────

/// `value * price * 10^exponent`, matching the program's `mul_oracle`.
pub fn mul_oracle(value: u128, price: u128, exponent: i32) -> Result<u128, TradingVenueError> {
    let raw = value
        .checked_mul(price)
        .ok_or_else(|| oracle_err("oracle mul overflow"))?;
    scale_pow10(raw, exponent)
}

/// `value / (price * 10^exponent)`, matching the program's `div_oracle`.
pub fn div_oracle(value: u128, price: u128, exponent: i32) -> Result<u128, TradingVenueError> {
    if exponent >= 0 {
        let divisor = price
            .checked_mul(
                10u128
                    .checked_pow(exponent as u32)
                    .ok_or_else(|| oracle_err("oracle scale overflow"))?,
            )
            .ok_or_else(|| oracle_err("oracle scale overflow"))?;
        value
            .checked_div(divisor)
            .ok_or_else(|| oracle_err("oracle divide by zero"))
    } else {
        value
            .checked_mul(
                10u128
                    .checked_pow(exponent.unsigned_abs())
                    .ok_or_else(|| oracle_err("oracle scale overflow"))?,
            )
            .ok_or_else(|| oracle_err("oracle scale overflow"))?
            .checked_div(price)
            .ok_or_else(|| oracle_err("oracle divide by zero"))
    }
}

/// The real-valued price as an `f64` (`price * 10^exponent`), for the marginal
/// spot price used by [`crate::trading_venue::QuoteResult::price`].
pub fn real_price_f64(price: u128, exponent: i32) -> f64 {
    price as f64 * 10f64.powi(exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doppler_account(price: u64, precision: u8) -> Vec<u8> {
        let mut d = vec![0u8; DOPPLER_LEN];
        d[8..16].copy_from_slice(&price.to_le_bytes());
        d[16] = precision;
        d
    }

    fn chainlink_push_account(answer: i128, decimals: u8) -> Vec<u8> {
        let mut d = vec![0u8; CL_HEADER_END + CL_ENTRY_LEN];
        d[0..8].copy_from_slice(&CL_DISCRIMINATOR);
        d[CL_DECIMALS] = decimals;
        d[CL_T_ANSWER..CL_T_ANSWER + 16].copy_from_slice(&answer.to_le_bytes());
        d
    }

    fn stream_account(feed_id: [u8; 32], price: u128, exponent: i32, conf: u16) -> Vec<u8> {
        let mut d = vec![0u8; STREAM_PRICE_LEN];
        d[0..8].copy_from_slice(&STREAM_PRICE_DISCRIMINATOR);
        d[SP_FEED_ID..SP_FEED_ID + 32].copy_from_slice(&feed_id);
        d[SP_PRICE..SP_PRICE + 16].copy_from_slice(&price.to_le_bytes());
        d[SP_EXPONENT..SP_EXPONENT + 4].copy_from_slice(&exponent.to_le_bytes());
        d[SP_CONFIDENCE_BPS..SP_CONFIDENCE_BPS + 2].copy_from_slice(&conf.to_le_bytes());
        d
    }

    fn leg(oracle: [u8; 32], feed_id: [u8; 32], provider: OracleProvider, inverse: bool, decimals: u8) -> OracleLeg {
        OracleLeg {
            oracle: Pubkey::new_from_array(oracle),
            feed_id,
            inverse,
            provider,
            decimals,
        }
    }

    #[test]
    fn doppler_read_matches_main() {
        let p = read_doppler(&doppler_account(1_000_000, 6)).unwrap();
        assert_eq!(p.price, 1_000_000);
        assert_eq!(p.exponent, -6);
        assert!(p.confidence_bps.is_none());
        assert!(read_doppler(&doppler_account(0, 6)).is_err());
    }

    #[test]
    fn chainlink_push_read() {
        let p = read_chainlink_push(&chainlink_push_account(99_000_000, 8)).unwrap();
        assert_eq!(p.price, 99_000_000);
        assert_eq!(p.exponent, -8);
    }

    #[test]
    fn single_straight_leg_passes_through_unchanged() {
        // This is the existing single-leg USDC+ Doppler path: composition is a no-op,
        // so a resolved asset yields the exact (price, exponent) `main` used.
        let only = OraclePrice { price: 1_000_000, exponent: -6, confidence_bps: None };
        let composed = compose_oracle_legs(&[(only, false)]).unwrap();
        assert_eq!(composed.price, 1_000_000);
        assert_eq!(composed.exponent, -6);
    }

    #[test]
    fn two_leg_product_composes_at_1e18() {
        let a = OraclePrice { price: 1_000_000, exponent: -6, confidence_bps: None }; // 1.0
        let b = OraclePrice { price: 99_000_000, exponent: -8, confidence_bps: Some(10) }; // 0.99
        let composed = compose_oracle_legs(&[(a, false), (b, false)]).unwrap();
        assert_eq!(composed.exponent, COMPOSITION_EXPONENT);
        assert_eq!(composed.price, 990_000_000_000_000_000); // 0.99 * 1e18
        assert_eq!(composed.confidence_bps, Some(10));
    }

    #[test]
    fn inverse_leg_divides() {
        let a = OraclePrice { price: 1_000_000, exponent: -6, confidence_bps: None }; // 1.0
        let b = OraclePrice { price: 99_000_000, exponent: -8, confidence_bps: None }; // 0.99
        let composed = compose_oracle_legs(&[(a, false), (b, true)]).unwrap();
        assert_eq!(composed.price, 1_010_101_010_101_010_101); // floor(1 / 0.99 * 1e18)
    }

    #[test]
    fn mul_div_oracle_roundtrip_signed_exponent() {
        let price = 990_000_000_000_000_000u128; // 0.99e18
        assert_eq!(mul_oracle(1000, price, -18).unwrap(), 990);
        assert_eq!(div_oracle(990, price, -18).unwrap(), 1000);
        // positive exponent path
        assert_eq!(mul_oracle(5, 3, 2).unwrap(), 1500);
        assert_eq!(div_oracle(1500, 3, 2).unwrap(), 5);
    }

    #[test]
    fn stream_price_reads_binds_feed_and_scale() {
        let feed = [7u8; 32];
        let acct = stream_account(feed, 100_000_000, -8, 25);
        let p = read_stream_price(&acct, &feed, 8).unwrap();
        assert_eq!(p.price, 100_000_000);
        assert_eq!(p.exponent, -8);
        assert_eq!(p.confidence_bps, Some(25));
        // v7 rate schema drops the confidence measure.
        assert_eq!(read_stream_rate(&acct, &feed, 8).unwrap().confidence_bps, None);
        // wrong feed id and wrong declared scale are rejected.
        assert!(read_stream_price(&acct, &[9u8; 32], 8).is_err());
        assert!(read_stream_price(&acct, &feed, 6).is_err());
    }

    fn build_asset(legs: &[OracleLeg]) -> Vec<u8> {
        let mut d = vec![0u8; ASSET_LEN];
        d[ASSET_VERSION_OFFSET] = ASSET_VERSION;
        d[ASSET_LEG_COUNT_OFFSET] = legs.len() as u8;
        for (i, l) in legs.iter().enumerate() {
            let base = ASSET_LEGS_OFFSET + i * LEG_LEN;
            d[base..base + 32].copy_from_slice(l.oracle.as_ref());
            d[base + 32..base + 64].copy_from_slice(&l.feed_id);
            d[base + 68] = l.inverse as u8;
            d[base + 69] = match l.provider {
                OracleProvider::Unset => 0,
                OracleProvider::Doppler => 1,
                OracleProvider::Pyth => 2,
                OracleProvider::ChainlinkPush => 3,
                OracleProvider::ChainlinkStreamFeed => 4,
                OracleProvider::ChainlinkStreamRate => 5,
            };
            d[base + 72] = l.decimals;
        }
        d
    }

    #[test]
    fn asset_legs_parse_and_order() {
        let l0 = leg([1; 32], [1; 32], OracleProvider::Doppler, false, 0);
        let l1 = leg([2; 32], [2; 32], OracleProvider::ChainlinkStreamFeed, true, 8);
        let legs = parse_asset_legs(&build_asset(&[l0, l1])).unwrap();
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].oracle, l0.oracle);
        assert_eq!(legs[0].provider, OracleProvider::Doppler);
        assert_eq!(legs[1].provider, OracleProvider::ChainlinkStreamFeed);
        assert!(legs[1].inverse);
        assert_eq!(legs[1].decimals, 8);
        assert_eq!(leg_oracle_accounts(&legs), vec![l0.oracle, l1.oracle]);
        // wrong version rejected
        let mut bad = build_asset(&[l0]);
        bad[ASSET_VERSION_OFFSET] = 1;
        assert!(parse_asset_legs(&bad).is_err());
    }

    #[test]
    fn resolve_single_leg_untagged_doppler_via_owner() {
        // provider Unset dispatches on the account owner — the real USDC+ shape.
        let oracle = [5u8; 32];
        let legs = vec![leg(oracle, oracle, OracleProvider::Unset, false, 0)];
        let accounts = vec![(DOPPLER_ORACLE_PROGRAM_ID, doppler_account(1_000_000, 6))];
        let composed = resolve_price(&legs, &accounts).unwrap();
        assert_eq!(composed.price, 1_000_000);
        assert_eq!(composed.exponent, -6);
    }

    #[test]
    fn resolve_two_leg_doppler_times_stream() {
        let feed = [7u8; 32];
        let legs = vec![
            leg([5; 32], [5; 32], OracleProvider::Doppler, false, 0),
            leg([6; 32], feed, OracleProvider::ChainlinkStreamFeed, false, 8),
        ];
        let dummy = Pubkey::new_from_array([0; 32]);
        let accounts = vec![
            (dummy, doppler_account(1_000_000, 6)),          // 1.0
            (dummy, stream_account(feed, 99_000_000, -8, 10)), // 0.99
        ];
        let composed = resolve_price(&legs, &accounts).unwrap();
        assert_eq!(composed.price, 990_000_000_000_000_000);
        assert_eq!(composed.exponent, -18);
        assert_eq!(composed.confidence_bps, Some(10));
    }
}
