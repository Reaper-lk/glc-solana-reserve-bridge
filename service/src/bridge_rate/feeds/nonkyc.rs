//! NonKYC market feed (docs/38-elastic-bridge-rate.md, feed manifest A).
//!
//! `GET {base_url}/market/getbysymbol/{market}` — the endpoint NonKYC's
//! own client library calls (the `nonkycapinode` package,
//! `nonkycApi.js`, `market/getbysymbol/` against
//! `https://api.nonkyc.io/api/v2`), with the market spelled `GLC_USDT`
//! (the URL-safe form the endpoint accepts alongside `GLC%2FUSDT`). The
//! response is one market document; the fields read are:
//!
//! | field | type | use |
//! |---|---|---|
//! | `lastPrice` | decimal string | the price, quote asset per base asset |
//! | `updatedAt` | integer, unix ms | the sample's `feed_at` |
//! | `isActive`, `isPaused` | bool | a market that is inactive or paused yields no sample |
//! | `symbol` | string | cross-checked against the configured market |
//!
//! Every USDT-quoted market is taken at par with USD (the manifest's
//! stated assumption). `lastPriceNumber` and the other float-typed
//! duplicates in the document are deliberately never read: the string is
//! the exact figure, and it goes through `decimal::parse_price_e12`.
//!
//! The same client serves the `ETH_USDT` market as the ETH/USD leg of the
//! Robinhood rail (`uniswap_v4`).

use serde::Deserialize;

use super::{millis_to_secs, FeedError, FeedHttp, PriceFeed};
use crate::bridge_rate::decimal::parse_price_e12;
use crate::bridge_rate::smoothing::Sample;
use crate::routes::Chain;

#[derive(Debug, Clone)]
pub struct NonKycMarket {
    pub base_url: String,
    /// `GLC_USDT`, `ETH_USDT` — base and quote joined by `_`.
    pub market: String,
}

impl NonKycMarket {
    fn url(&self) -> String {
        format!(
            "{}/market/getbysymbol/{}",
            self.base_url.trim_end_matches('/'),
            self.market
        )
    }

    /// The market's expected `symbol` as the document spells it.
    fn expected_symbol(&self) -> String {
        self.market.replacen('_', "/", 1)
    }

    /// One poll: the market's last price and its update instant.
    pub async fn fetch_price(&self, http: &FeedHttp) -> Result<Sample, FeedError> {
        let body = http.get_bounded(&self.url()).await?;
        let doc = parse_market_document(&body, &self.expected_symbol())?;
        Ok(Sample {
            feed_at: doc.updated_at_secs,
            observed_at: super::now_unix(),
            price_e12: doc.price_e12,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketPrice {
    pub price_e12: u64,
    pub updated_at_secs: i64,
    pub last_trade_at_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct MarketDocument {
    symbol: String,
    #[serde(rename = "lastPrice")]
    last_price: String,
    #[serde(rename = "updatedAt")]
    updated_at: i64,
    #[serde(rename = "lastTradeAt", default)]
    last_trade_at: Option<i64>,
    #[serde(rename = "isActive")]
    is_active: bool,
    #[serde(rename = "isPaused")]
    is_paused: bool,
}

/// Strict parse of one market document.
pub fn parse_market_document(body: &[u8], expected_symbol: &str) -> Result<MarketPrice, FeedError> {
    let doc: MarketDocument = serde_json::from_slice(body)
        .map_err(|e| FeedError::Malformed(format!("market document: {e}")))?;
    if doc.symbol != expected_symbol {
        return Err(FeedError::Malformed(format!(
            "market document is for {:?}, expected {expected_symbol:?}",
            doc.symbol
        )));
    }
    if !doc.is_active || doc.is_paused {
        return Err(FeedError::MarketUnusable(format!(
            "{}: isActive={} isPaused={}",
            doc.symbol, doc.is_active, doc.is_paused
        )));
    }
    if doc.updated_at <= 0 {
        return Err(FeedError::Malformed(format!(
            "updatedAt {} is not a timestamp",
            doc.updated_at
        )));
    }
    let price_e12 = parse_price_e12(&doc.last_price)?;
    Ok(MarketPrice {
        price_e12,
        updated_at_secs: millis_to_secs(doc.updated_at),
        last_trade_at_secs: doc.last_trade_at.map(millis_to_secs),
    })
}

/// The Goldcoin L1 rail: one NonKYC market.
pub struct NonKycFeed {
    pub http: FeedHttp,
    pub market: NonKycMarket,
}

impl PriceFeed for NonKycFeed {
    fn chain(&self) -> Chain {
        Chain::Goldcoin
    }

    fn describe(&self) -> String {
        format!("nonkyc {} at {}", self.market.market, self.market.base_url)
    }

    fn fetch(&self, _now: i64) -> super::BoxFut<'_, Result<Sample, FeedError>> {
        Box::pin(async move { self.market.fetch_price(&self.http).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{"_id":"6a837ed52e7a07c6135be33f","symbol":"GLC/USDT","primaryName":"Goldcoin",
        "primaryTicker":"GLC","lastPrice":"0.000156705","yesterdayPrice":"0.000090647",
        "lastTradeAt":1789429916125,"priceDecimals":9,"isActive":true,"isPaused":false,
        "bestAsk":"0.000157351","bestBid":"0.000156599","updatedAt":1789429924581,
        "lastPriceNumber":0.000156705,"secondaryUsdValue":"1.00000"}"#;

    #[test]
    fn the_real_market_document_parses_to_the_exact_last_price() {
        let p = parse_market_document(DOC.as_bytes(), "GLC/USDT").unwrap();
        assert_eq!(p.price_e12, 156_705_000);
        assert_eq!(p.updated_at_secs, 1_789_429_924);
        assert_eq!(p.last_trade_at_secs, Some(1_789_429_916));
    }

    #[test]
    fn the_wrong_market_a_paused_market_and_a_bad_price_are_refused() {
        assert!(matches!(
            parse_market_document(DOC.as_bytes(), "ETH/USDT"),
            Err(FeedError::Malformed(_))
        ));
        let paused = DOC.replace("\"isPaused\":false", "\"isPaused\":true");
        assert!(matches!(
            parse_market_document(paused.as_bytes(), "GLC/USDT"),
            Err(FeedError::MarketUnusable(_))
        ));
        let inactive = DOC.replace("\"isActive\":true", "\"isActive\":false");
        assert!(matches!(
            parse_market_document(inactive.as_bytes(), "GLC/USDT"),
            Err(FeedError::MarketUnusable(_))
        ));
        for bad in ["\"0\"", "\"-1\"", "\"abc\"", "\"\"", "\"99999999999\""] {
            let doc = DOC.replace("\"0.000156705\"", bad);
            assert!(
                matches!(
                    parse_market_document(doc.as_bytes(), "GLC/USDT"),
                    Err(FeedError::Price(_))
                ),
                "{bad}"
            );
        }
        // A number where the string is expected is a shape error.
        let numeric = DOC.replace("\"0.000156705\"", "0.000156705");
        assert!(matches!(
            parse_market_document(numeric.as_bytes(), "GLC/USDT"),
            Err(FeedError::Malformed(_))
        ));
        assert!(matches!(
            parse_market_document(b"not json", "GLC/USDT"),
            Err(FeedError::Malformed(_))
        ));
        assert!(matches!(
            parse_market_document(b"{}", "GLC/USDT"),
            Err(FeedError::Malformed(_))
        ));
    }

    #[test]
    fn the_url_is_the_documented_endpoint() {
        let m = NonKycMarket {
            base_url: "https://api.nonkyc.io/api/v2/".to_string(),
            market: "GLC_USDT".to_string(),
        };
        assert_eq!(
            m.url(),
            "https://api.nonkyc.io/api/v2/market/getbysymbol/GLC_USDT"
        );
        assert_eq!(m.expected_symbol(), "GLC/USDT");
    }
}
