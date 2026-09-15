//! Jupiter Price API v3 feed (docs/38-elastic-bridge-rate.md, feed
//! manifest B).
//!
//! `GET {base_url}?ids={mint}` — Jupiter's current price API
//! (`developers.jup.ag/docs/api-reference/price/v3/price`: "GET
//! https://api.jup.ag/price/v3?ids={mints}", up to 50 ids). The keyless
//! host `https://lite-api.jup.ag/price/v3` answers the identical document
//! and is the manifest's default; `api.jup.ag` accepts an `x-api-key`,
//! which this feed never sends (no secrets). The response is an object
//! keyed by mint:
//!
//! ```json
//! {"<mint>":{"usdPrice":0.0000443670077,"blockId":447085739,"decimals":6,
//!            "liquidity":9033.73,"priceChange24h":4.96,"createdAt":"…"}}
//! ```
//!
//! `usdPrice` is a JSON NUMBER, not a string. It is read as raw text
//! (`serde_json::value::RawValue`) and parsed by `decimal::parse_price_e12`
//! — never through `f64`. A mint absent from the response ("tokens
//! without reliable pricing are omitted") is a refusal, not a zero.
//!
//! Jupiter's document carries no timestamp, only `blockId`, the Solana
//! slot its last-swap price was computed at. `feed_at` is therefore the
//! receipt instant, and `blockId` is logged with every sample — see the
//! manifest for why the slot's own age is not used as the staleness bound.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{FeedError, FeedHttp, PriceFeed};
use crate::bridge_rate::decimal::parse_price_e12;
use crate::bridge_rate::smoothing::Sample;
use crate::routes::Chain;

#[derive(Debug, Clone)]
pub struct JupiterFeed {
    pub http: FeedHttp,
    pub base_url: String,
    pub mint: String,
}

#[derive(Debug, Deserialize)]
struct PriceEntry<'a> {
    #[serde(rename = "usdPrice", borrow)]
    usd_price: &'a RawValue,
    #[serde(rename = "blockId", default)]
    block_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JupiterPrice {
    pub price_e12: u64,
    pub block_id: Option<u64>,
}

/// Strict parse of the v3 document for one mint.
pub fn parse_price_document(body: &[u8], mint: &str) -> Result<JupiterPrice, FeedError> {
    let text = std::str::from_utf8(body)
        .map_err(|e| FeedError::Malformed(format!("price document is not UTF-8: {e}")))?;
    let doc: BTreeMap<String, Option<PriceEntry<'_>>> = serde_json::from_str(text)
        .map_err(|e| FeedError::Malformed(format!("price document: {e}")))?;
    let entry = doc.get(mint).and_then(|e| e.as_ref()).ok_or_else(|| {
        FeedError::Malformed(format!("mint {mint} absent from the price document"))
    })?;
    let raw = entry.usd_price.get();
    if raw.starts_with('"') {
        return Err(FeedError::Malformed(format!(
            "usdPrice is a string ({raw}), expected a JSON number"
        )));
    }
    let price_e12 = parse_price_e12(raw)?;
    Ok(JupiterPrice {
        price_e12,
        block_id: entry.block_id,
    })
}

impl JupiterFeed {
    fn url(&self) -> String {
        format!("{}?ids={}", self.base_url.trim_end_matches('/'), self.mint)
    }
}

impl PriceFeed for JupiterFeed {
    fn chain(&self) -> Chain {
        Chain::Solana
    }

    fn describe(&self) -> String {
        format!("jupiter price v3 for {} at {}", self.mint, self.base_url)
    }

    fn fetch(&self, now: i64) -> super::BoxFut<'_, Result<Sample, FeedError>> {
        Box::pin(async move {
            let body = self.http.get_bounded(&self.url()).await?;
            let price = parse_price_document(&body, &self.mint)?;
            tracing::debug!(
                mint = %self.mint,
                block_id = ?price.block_id,
                price_e12 = price.price_e12,
                "jupiter price sample"
            );
            Ok(Sample {
                feed_at: now,
                observed_at: now,
                price_e12: price.price_e12,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINT: &str = "Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump";
    const DOC: &str = r#"{"Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump":{"createdAt":"2026-08-09T00:05:32Z","liquidity":9033.739981006463,"usdPrice":0.000044367007702948316,"blockId":447085739,"decimals":6,"priceChange24h":4.9600503361648975,"launchpad":"pump.fun"}}"#;

    #[test]
    fn the_real_document_parses_the_number_text_exactly() {
        let p = parse_price_document(DOC.as_bytes(), MINT).unwrap();
        assert_eq!(
            p.price_e12, 44_367_007,
            "truncated at 12 places, never rounded"
        );
        assert_eq!(p.block_id, Some(447_085_739));
    }

    #[test]
    fn exponent_notation_and_integers_are_accepted_as_numbers() {
        let doc = DOC.replace("0.000044367007702948316", "4.4367e-5");
        assert_eq!(
            parse_price_document(doc.as_bytes(), MINT)
                .unwrap()
                .price_e12,
            44_367_000
        );
        let doc = DOC.replace("0.000044367007702948316", "2");
        assert_eq!(
            parse_price_document(doc.as_bytes(), MINT)
                .unwrap()
                .price_e12,
            2_000_000_000_000
        );
    }

    #[test]
    fn an_absent_mint_a_null_entry_a_string_price_and_bad_numbers_are_refused() {
        assert!(matches!(
            parse_price_document(DOC.as_bytes(), "OtherMint111111111111111111111111111111111"),
            Err(FeedError::Malformed(_))
        ));
        let null = format!(r#"{{"{MINT}":null}}"#);
        assert!(matches!(
            parse_price_document(null.as_bytes(), MINT),
            Err(FeedError::Malformed(_))
        ));
        assert!(matches!(
            parse_price_document(b"{}", MINT),
            Err(FeedError::Malformed(_))
        ));
        let string = DOC.replace("0.000044367007702948316", "\"0.00004\"");
        assert!(matches!(
            parse_price_document(string.as_bytes(), MINT),
            Err(FeedError::Malformed(_))
        ));
        for bad in ["0", "0.0", "-0.5", "1e400", "99999999999"] {
            let doc = DOC.replace("0.000044367007702948316", bad);
            assert!(
                matches!(
                    parse_price_document(doc.as_bytes(), MINT),
                    Err(FeedError::Price(_))
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            parse_price_document(b"\xff\xfe", MINT),
            Err(FeedError::Malformed(_))
        ));
    }
}
