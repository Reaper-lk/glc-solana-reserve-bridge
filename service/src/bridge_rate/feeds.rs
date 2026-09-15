//! The live price feeds (docs/38-elastic-bridge-rate.md, Phase 2B "Feed
//! manifest"): one client per rail behind one small trait, and the poller
//! that drives them into the [`LiveBook`].
//!
//! | Rail | Source | Module |
//! |---|---|---|
//! | Goldcoin L1 | NonKYC `GET /api/v2/market/getbysymbol/GLC_USDT` | [`nonkyc`] |
//! | Solana | Jupiter Price API v3, `GET /price/v3?ids=<mint>` | [`jupiter`] |
//! | Robinhood Chain | Uniswap v4 `StateView.getSlot0(poolId)` on-chain, × ETH/USD from NonKYC `ETH_USDT` | [`uniswap_v4`] |
//!
//! # What every client shares
//!
//! - **One hardened HTTP client** ([`FeedHttp`]): `https://` only,
//!   connect and request timeouts, redirects refused outright (a 3xx is a
//!   hard failure — a feed has no business sending this daemon anywhere
//!   else), and a bounded body read ([`MAX_RESPONSE_BODY_BYTES`]) so a
//!   misbehaving endpoint cannot balloon memory. No credentials of any
//!   kind are sent or logged.
//! - **Strict parsing.** A response is a typed document; unknown shape,
//!   missing fields, a non-numeric / zero / negative / overflowing price,
//!   or a feed that says its own market is paused is a [`FeedError`],
//!   and a `FeedError` is the ABSENCE of a sample — nothing is recorded.
//!   The book then ages into staleness on its own (`live.rs`).
//! - **No retries inside a poll.** The poller runs on a fixed cadence and
//!   a failed poll is simply reported; the next tick tries again. There
//!   is no retry loop that could hammer a struggling endpoint or hide an
//!   outage behind eventual success.
//!
//! # Feed timestamps
//!
//! The `feed_at` a client stamps on a sample is the instant the SOURCE
//! says its data is current: NonKYC's market `updatedAt`; the Robinhood
//! head block's timestamp (and the ETH/USD market's `updatedAt`, whichever
//! is older); and, for Jupiter — which returns no timestamp, only the
//! Solana `blockId` its last-swap price came from — the moment the
//! response was received. That last choice is deliberate and documented
//! in the manifest: Jupiter's price is by construction "the last swap",
//! and for a thin market the last swap can be hours old while the served
//! price is nonetheless the market's current one. `blockId` is logged on
//! every sample for audit.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::decimal::PriceParseError;
use super::live::LiveBook;
use super::smoothing::Sample;
use crate::routes::Chain;

pub mod jupiter;
pub mod nonkyc;
pub mod uniswap_v4;

#[cfg(test)]
mod tests;

/// A feed response larger than this is refused unread. The largest
/// legitimate response (a NonKYC market document) is a few kilobytes.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a poll produced no sample.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("timed out")]
    Timeout,
    #[error("HTTP {status}")]
    Status { status: u16 },
    #[error("malformed response: {0}")]
    Malformed(String),
    #[error("price refused: {0}")]
    Price(#[from] PriceParseError),
    #[error("the source reports its market as unusable: {0}")]
    MarketUnusable(String),
    #[error("derived price overflowed: {0}")]
    Overflow(String),
}

/// One rail's feed.
pub trait PriceFeed: Send + Sync {
    fn chain(&self) -> Chain;
    /// A one-line, secret-free description for logs and status.
    fn describe(&self) -> String;
    /// One poll. `now` is the receipt instant to stamp on a sample whose
    /// source carries no timestamp of its own.
    fn fetch(&self, now: i64) -> BoxFut<'_, Result<Sample, FeedError>>;
}

/// The hardened HTTP client every HTTP-backed feed uses.
#[derive(Debug, Clone)]
pub struct FeedHttp {
    client: reqwest::Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedHttpConfig {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for FeedHttpConfig {
    fn default() -> Self {
        FeedHttpConfig {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
        }
    }
}

impl FeedHttp {
    pub fn new(config: FeedHttpConfig) -> Result<FeedHttp, String> {
        Self::build(config, true)
    }

    /// A client that also speaks plain `http://`, for tests against a
    /// loopback server. **Tests only** — production construction goes
    /// through [`FeedHttp::new`], and the config loader additionally
    /// refuses any feed URL that is not `https://`.
    #[doc(hidden)]
    pub fn new_plain_for_tests(config: FeedHttpConfig) -> Result<FeedHttp, String> {
        Self::build(config, false)
    }

    fn build(config: FeedHttpConfig, https_only: bool) -> Result<FeedHttp, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(https_only)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(FeedHttp { client })
    }

    /// `GET url`, expecting a 2xx JSON body no larger than
    /// [`MAX_RESPONSE_BODY_BYTES`]. Returns the raw body; the caller
    /// parses it strictly.
    pub async fn get_bounded(&self, url: &str) -> Result<Vec<u8>, FeedError> {
        let resp = self
            .client
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    FeedError::Timeout
                } else {
                    FeedError::Transport(redact(&e.to_string()))
                }
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FeedError::Status {
                status: status.as_u16(),
            });
        }
        if let Some(len) = resp.content_length() {
            if len > MAX_RESPONSE_BODY_BYTES as u64 {
                return Err(FeedError::Malformed(format!(
                    "Content-Length {len} exceeds the {MAX_RESPONSE_BODY_BYTES}-byte limit"
                )));
            }
        }
        let mut resp = resp;
        let mut buf = Vec::new();
        loop {
            let chunk = resp.chunk().await.map_err(|e| {
                if e.is_timeout() {
                    FeedError::Timeout
                } else {
                    FeedError::Transport(redact(&e.to_string()))
                }
            })?;
            let Some(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_RESPONSE_BODY_BYTES {
                return Err(FeedError::Malformed(format!(
                    "body exceeded the {MAX_RESPONSE_BODY_BYTES}-byte limit"
                )));
            }
        }
        Ok(buf)
    }
}

/// Strips anything that looks like a URL query from an error string, so
/// an endpoint key embedded in a configured URL never reaches a log line
/// through a transport error message.
pub(crate) fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_query = false;
    for c in text.chars() {
        if c == '?' {
            in_query = true;
            out.push_str("?<redacted>");
            continue;
        }
        if in_query && (c == ' ' || c == ')' || c == '"') {
            in_query = false;
        }
        if !in_query {
            out.push(c);
        }
    }
    out
}

/// Milliseconds since the epoch (as feeds print them) to whole seconds.
pub(crate) fn millis_to_secs(ms: i64) -> i64 {
    ms.div_euclid(1_000)
}

/// The poller: on every tick, polls each feed once and records the
/// outcome in `book`. Runs until `shutdown` fires. A single feed's
/// failure never delays or blocks the others.
pub async fn run_poller(
    book: Arc<LiveBook>,
    feeds: Vec<Box<dyn PriceFeed>>,
    poll_interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("bridge-rate feed poller stopping");
                    return;
                }
            }
        }
        let now = now_unix();
        for feed in &feeds {
            poll_one(&book, feed.as_ref(), now).await;
        }
    }
}

/// One feed, one poll, one recorded outcome.
pub async fn poll_one(book: &LiveBook, feed: &dyn PriceFeed, now: i64) {
    match feed.fetch(now).await {
        Ok(sample) => {
            let kept = book.record_sample(feed.chain(), sample);
            tracing::info!(
                rail = feed.chain().as_str(),
                price_e12 = sample.price_e12,
                feed_at = sample.feed_at,
                observed_at = sample.observed_at,
                kept,
                "bridge-rate sample"
            );
        }
        Err(e) => {
            let detail = e.to_string();
            tracing::warn!(
                rail = feed.chain().as_str(),
                feed = %feed.describe(),
                error = %detail,
                "bridge-rate feed poll failed; no sample recorded"
            );
            book.record_failure(feed.chain(), now, detail);
        }
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn redaction_strips_query_strings_from_error_text() {
        assert_eq!(
            redact("error sending request for url (https://x.example/v2/abc?key=SECRET)"),
            "error sending request for url (https://x.example/v2/abc?<redacted>)"
        );
        assert_eq!(redact("plain"), "plain");
    }

    #[test]
    fn millis_floor_to_seconds() {
        assert_eq!(millis_to_secs(1_789_429_916_125), 1_789_429_916);
        assert_eq!(millis_to_secs(999), 0);
    }

    #[test]
    fn the_http_client_refuses_plain_http_and_is_bounded() {
        let http = FeedHttp::new(FeedHttpConfig::default()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(http.get_bounded("http://127.0.0.1:9/never"))
            .unwrap_err();
        assert!(matches!(err, FeedError::Transport(_)), "{err}");
    }
}
