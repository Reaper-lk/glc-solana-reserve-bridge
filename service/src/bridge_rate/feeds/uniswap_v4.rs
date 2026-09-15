//! Uniswap v4 pool feed for Robinhood Chain (docs/38-elastic-bridge-
//! rate.md, feed manifest C).
//!
//! The Robinhood GLC market is a Uniswap **v4** pool — a `PoolId`
//! (`bytes32`) inside the singleton `PoolManager`, not a contract of its
//! own — paired against NATIVE ETH. Its state is read through the official
//! `StateView` lens with two `eth_call`s per poll:
//!
//! ```text
//! StateView.getSlot0(poolId)     -> (sqrtPriceX96 uint160, tick int24, protocolFee uint24, lpFee uint24)
//! StateView.getLiquidity(poolId) -> liquidity uint128   // zero = no market, no sample
//! ```
//!
//! plus `eth_getBlockByNumber(latest)` for the head timestamp, which is
//! the sample's `feed_at` (the chain's own clock for how current the state
//! is — no Robinhood-side feed carries a fresher one).
//!
//! # From `sqrtPriceX96` to a USD price, exactly
//!
//! Uniswap defines `sqrtPriceX96 = sqrt(P) · 2^96` where `P` is the raw
//! price of `currency0` in units of `currency1` (wei per wei). With ETH as
//! `currency0` and GLC as `currency1`, `P` is GLC-wei per ETH-wei, so one
//! GLC is worth `1/P` ETH and
//!
//! ```text
//! GLC_usd = ETH_usd / P = ETH_usd · 2^192 / sqrtPriceX96²   (× 10^(dec1 − dec0), both 18 here)
//! ```
//!
//! `sqrtPriceX96²` is up to 320 bits, so the division is done in two exact
//! 256÷128 steps that keep every intermediate inside 256 bits:
//!
//! ```text
//! a           = floor(ETH_usd_e12 · 2^96 / sqrtPriceX96)
//! GLC_usd_e12 = floor(a · 2^96 / sqrtPriceX96)
//! ```
//!
//! Two floors instead of one lose at most one unit of the final figure
//! plus `2^96 / sqrtPriceX96` (< 1 whenever `sqrtPriceX96 > 2^96`, i.e.
//! whenever one ETH buys more than one GLC-wei). Deterministic, integer,
//! and independent of which process runs it. The mirror formula applies
//! when GLC is `currency0` (`glc_is_currency1 = false`).
//!
//! ETH/USD comes from the NonKYC `ETH_USDT` market (the manifest's
//! minimal conversion: the same client and host the Goldcoin rail already
//! depends on; USDT at par with USD). The sample's `feed_at` is the OLDER
//! of the head block's timestamp and that market's `updatedAt`, so a
//! stale leg on either side ages the whole rail.

use super::nonkyc::NonKycMarket;
use super::{FeedError, FeedHttp, PriceFeed};
use crate::bridge_rate::bigmath::{mul_div, U256};
use crate::bridge_rate::smoothing::Sample;
use crate::evm::abi::{return_words, word_bytes32, Calldata};
use crate::evm::{EvmAddress, EvmU256};
use crate::robinhood::rpc::{EvmBlockTag, EvmCall, EvmCallRpc, EvmRpc};
use crate::routes::Chain;

/// The pool, as verified on-chain from its `Initialize` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniswapV4Pool {
    pub state_view: EvmAddress,
    pub pool_id: [u8; 32],
    /// `true` when GLC is `currency1` (ETH is `currency0`) — the verified
    /// layout of the Robinhood pool.
    pub glc_is_currency1: bool,
    pub currency0_decimals: u8,
    pub currency1_decimals: u8,
}

pub struct UniswapV4Feed<R> {
    pub rpc: R,
    pub pool: UniswapV4Pool,
    pub http: FeedHttp,
    pub eth_usd: NonKycMarket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot0 {
    pub sqrt_price_x96: u128,
    pub liquidity: u128,
}

/// The GLC/USD price from the pool's `sqrtPriceX96` and the ETH/USD
/// price. Pure and exact — see the module docs for the derivation.
pub fn glc_usd_e12_from_sqrt_price(
    sqrt_price_x96: u128,
    eth_usd_e12: u64,
    glc_is_currency1: bool,
    currency0_decimals: u8,
    currency1_decimals: u8,
) -> Result<u64, FeedError> {
    if sqrt_price_x96 == 0 {
        return Err(FeedError::Malformed("sqrtPriceX96 is zero".to_string()));
    }
    if eth_usd_e12 == 0 {
        return Err(FeedError::Malformed("ETH/USD price is zero".to_string()));
    }
    let overflow = |what: &str| FeedError::Overflow(what.to_string());
    let s = sqrt_price_x96;
    // Raw price of GLC in ETH-wei per GLC-wei, times ETH_usd_e12:
    //   currency1 = GLC: eth_usd * 2^192 / s^2
    //   currency0 = GLC: eth_usd * s^2 / 2^192
    let raw: u128 = if glc_is_currency1 {
        let a = U256::from_u128(u128::from(eth_usd_e12))
            .shift_left(96)
            .div_u128(s)
            .ok_or_else(|| overflow("eth_usd·2^96/sqrtPrice"))?;
        U256::from_u128(a)
            .shift_left(96)
            .div_u128(s)
            .ok_or_else(|| overflow("a·2^96/sqrtPrice"))?
    } else {
        let a = U256::mul_u128(u128::from(eth_usd_e12), s)
            .div_u128(1u128 << 96)
            .ok_or_else(|| overflow("eth_usd·sqrtPrice/2^96"))?;
        U256::mul_u128(a, s)
            .div_u128(1u128 << 96)
            .ok_or_else(|| overflow("a·sqrtPrice/2^96"))?
    };
    // Decimal adjustment: the raw figure is per-wei; a human unit of GLC
    // is 10^dec_glc wei and one of ETH is 10^dec_eth wei.
    let (glc_dec, eth_dec) = if glc_is_currency1 {
        (currency1_decimals, currency0_decimals)
    } else {
        (currency0_decimals, currency1_decimals)
    };
    let adjusted = if glc_dec >= eth_dec {
        mul_div(raw, 10u128.pow(u32::from(glc_dec - eth_dec)), 1)
    } else {
        mul_div(raw, 1, 10u128.pow(u32::from(eth_dec - glc_dec)))
    }
    .ok_or_else(|| overflow("decimal adjustment"))?;
    if adjusted == 0 {
        return Err(FeedError::Malformed(
            "derived GLC/USD price is below one unit at 12 decimals".to_string(),
        ));
    }
    u64::try_from(adjusted).map_err(|_| overflow("GLC/USD does not fit u64 at 12 decimals"))
}

fn word_to_u128(word: &[u8; 32], field: &str) -> Result<u128, FeedError> {
    EvmU256::try_from_be_slice(word)
        .map_err(|e| FeedError::Malformed(format!("{field}: {e}")))?
        .try_to_u128()
        .map_err(|_| FeedError::Overflow(format!("{field} exceeds u128")))
}

impl<R: EvmRpc + EvmCallRpc + Send + Sync> UniswapV4Feed<R> {
    /// `getSlot0` and `getLiquidity` at the latest block.
    pub async fn read_slot0(&self) -> Result<Slot0, FeedError> {
        let rpc_err = |e: crate::robinhood::rpc::EvmRpcError| {
            FeedError::Transport(super::redact(&e.to_string()))
        };
        let slot0_call = EvmCall {
            to: self.pool.state_view,
            data: Calldata::new("getSlot0(bytes32)")
                .word(word_bytes32(self.pool.pool_id))
                .finish(),
        };
        let raw = self
            .rpc
            .call(&slot0_call, EvmBlockTag::Latest)
            .await
            .map_err(rpc_err)?;
        let words = return_words::<4>(&raw)
            .map_err(|e| FeedError::Malformed(format!("getSlot0 return: {e}")))?;
        let sqrt_price_x96 = word_to_u128(&words[0], "sqrtPriceX96")?;
        let liquidity_call = EvmCall {
            to: self.pool.state_view,
            data: Calldata::new("getLiquidity(bytes32)")
                .word(word_bytes32(self.pool.pool_id))
                .finish(),
        };
        let raw = self
            .rpc
            .call(&liquidity_call, EvmBlockTag::Latest)
            .await
            .map_err(rpc_err)?;
        let words = return_words::<1>(&raw)
            .map_err(|e| FeedError::Malformed(format!("getLiquidity return: {e}")))?;
        let liquidity = word_to_u128(&words[0], "liquidity")?;
        Ok(Slot0 {
            sqrt_price_x96,
            liquidity,
        })
    }

    async fn head_timestamp(&self) -> Result<i64, FeedError> {
        let rpc_err = |e: crate::robinhood::rpc::EvmRpcError| {
            FeedError::Transport(super::redact(&e.to_string()))
        };
        let number = self.rpc.block_number().await.map_err(rpc_err)?;
        let block = self
            .rpc
            .block_by_number(number)
            .await
            .map_err(rpc_err)?
            .ok_or_else(|| FeedError::Malformed(format!("head block {number} vanished")))?;
        i64::try_from(block.timestamp)
            .map_err(|_| FeedError::Malformed("head block timestamp exceeds i64".to_string()))
    }
}

impl<R: EvmRpc + EvmCallRpc + Send + Sync> PriceFeed for UniswapV4Feed<R> {
    fn chain(&self) -> Chain {
        Chain::Robinhood
    }

    fn describe(&self) -> String {
        format!(
            "uniswap v4 pool 0x{} via StateView {} (glc_is_currency1={}), ETH/USD from nonkyc {}",
            hex(&self.pool.pool_id),
            self.pool.state_view,
            self.pool.glc_is_currency1,
            self.eth_usd.market
        )
    }

    fn fetch(&self, now: i64) -> super::BoxFut<'_, Result<Sample, FeedError>> {
        Box::pin(async move {
            let slot0 = self.read_slot0().await?;
            if slot0.liquidity == 0 {
                return Err(FeedError::MarketUnusable(
                    "the pool has no liquidity".to_string(),
                ));
            }
            let head_at = self.head_timestamp().await?;
            let eth = self.eth_usd.fetch_price(&self.http).await?;
            let price_e12 = glc_usd_e12_from_sqrt_price(
                slot0.sqrt_price_x96,
                eth.price_e12,
                self.pool.glc_is_currency1,
                self.pool.currency0_decimals,
                self.pool.currency1_decimals,
            )?;
            tracing::debug!(
                sqrt_price_x96 = slot0.sqrt_price_x96,
                liquidity = slot0.liquidity,
                eth_usd_e12 = eth.price_e12,
                head_at,
                price_e12,
                "uniswap v4 price sample"
            );
            Ok(Sample {
                feed_at: head_at.min(eth.feed_at),
                observed_at: now,
                price_e12,
            })
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool's live `sqrtPriceX96` on 2026-09-14 (StateView.getSlot0)
    /// and NonKYC's ETH/USDT at the same instant.
    const SQRT_PRICE_X96: u128 = 666_733_903_468_254_117_567_079_170_768_462;
    const ETH_USD_E12: u64 = 2_515_750_000_000_000;

    #[test]
    fn the_live_pool_state_derives_the_price_the_market_shows() {
        let p = glc_usd_e12_from_sqrt_price(SQRT_PRICE_X96, ETH_USD_E12, true, 18, 18).unwrap();
        // tick 180765 -> 1.0001^180765 ≈ 7.08e7 GLC per ETH -> ≈ $0.0000355
        assert!((35_000_000..36_500_000).contains(&p), "{p}");
    }

    #[test]
    fn the_derivation_is_exact_against_a_hand_computed_square() {
        // sqrtPriceX96 = 2^96 * 10 -> P = 100 GLC-wei per ETH-wei -> GLC = ETH/100
        let s = 10u128 << 96;
        assert_eq!(
            glc_usd_e12_from_sqrt_price(s, 1_000 * 1_000_000_000_000, true, 18, 18).unwrap(),
            10 * 1_000_000_000_000
        );
        // The mirror layout: GLC as currency0 -> GLC = ETH * 100
        assert_eq!(
            glc_usd_e12_from_sqrt_price(s, 1_000_000_000_000, false, 18, 18).unwrap(),
            100 * 1_000_000_000_000
        );
        // Decimals: an 18-decimal GLC against a 6-decimal quote asset
        // scales the raw per-wei figure up by 1e12. sqrtPrice = 1e6·2^96
        // -> P = 1e12 GLC-wei per quote-wei -> one GLC (1e18 wei) costs
        // 1e6 quote-wei = 1 quote unit = $1 at a $1 quote asset.
        let s = 1_000_000u128 << 96;
        assert_eq!(
            glc_usd_e12_from_sqrt_price(s, 1_000_000_000_000, true, 6, 18).unwrap(),
            1_000_000_000_000
        );
        // And a 6-decimal GLC against 18-decimal ETH at that same raw price
        // is worth less than one unit at 12 decimals: refused, not zero.
        assert!(matches!(
            glc_usd_e12_from_sqrt_price(10u128 << 96, 1_000_000_000_000, true, 18, 6),
            Err(FeedError::Malformed(_))
        ));
    }

    #[test]
    fn zero_state_and_overflow_are_refused() {
        assert!(matches!(
            glc_usd_e12_from_sqrt_price(0, ETH_USD_E12, true, 18, 18),
            Err(FeedError::Malformed(_))
        ));
        assert!(matches!(
            glc_usd_e12_from_sqrt_price(SQRT_PRICE_X96, 0, true, 18, 18),
            Err(FeedError::Malformed(_))
        ));
        // A sqrtPrice of 1 (P ≈ 0) values one GLC at ~2^192 ETH.
        assert!(matches!(
            glc_usd_e12_from_sqrt_price(1, ETH_USD_E12, true, 18, 18),
            Err(FeedError::Overflow(_))
        ));
        // An astronomically high P values GLC below one unit at 12 dp.
        assert!(matches!(
            glc_usd_e12_from_sqrt_price(u128::MAX, 1, true, 18, 18),
            Err(FeedError::Malformed(_))
        ));
    }

    #[test]
    fn the_calldata_is_the_state_view_abi() {
        let id = [0x11u8; 32];
        let data = Calldata::new("getSlot0(bytes32)")
            .word(word_bytes32(id))
            .finish();
        assert_eq!(data.len(), 4 + 32);
        assert_eq!(&data[..4], &crate::evm::abi::selector("getSlot0(bytes32)"));
        assert_eq!(&data[4..], &id);
    }
}
