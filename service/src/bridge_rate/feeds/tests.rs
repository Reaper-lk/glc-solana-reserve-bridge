//! Feed-client tests against a real loopback HTTP server and a scripted
//! EVM RPC: every failure class the manifest names (transport, timeout,
//! non-success status, malformed body, non-numeric / zero / negative /
//! overflowing price, a stale or paused market) yields NO sample, and the
//! poller records exactly that.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use super::jupiter::JupiterFeed;
use super::nonkyc::{NonKycFeed, NonKycMarket};
use super::uniswap_v4::{UniswapV4Feed, UniswapV4Pool};
use super::*;
use crate::bridge_rate::live::{LiveBook, LiveRateConfig};
use crate::evm::hash::EvmBlockHash;
use crate::evm::{EvmAddress, EvmChainId};
use crate::robinhood::rpc::{
    EvmBlockRef, EvmBlockTag, EvmCall, EvmCallRpc, EvmLogFilter, EvmRawLog, EvmRpc, EvmRpcError,
};
use crate::routes::Chain;

const GLC_DOC: &str = r#"{"symbol":"GLC/USDT","lastPrice":"0.000156705","lastTradeAt":1789429916125,"isActive":true,"isPaused":false,"updatedAt":1789429924581,"lastPriceNumber":0.000156705}"#;
const ETH_DOC: &str = r#"{"symbol":"ETH/USDT","lastPrice":"2515.75","lastTradeAt":1789429921346,"isActive":true,"isPaused":false,"updatedAt":1789429922535}"#;
const MINT: &str = "Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump";
const JUP_DOC: &str = r#"{"Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump":{"usdPrice":0.000044367007702948316,"blockId":447085739,"decimals":6}}"#;

/// What the loopback server answers, by path.
#[derive(Clone)]
enum Answer {
    Json(StatusCode, String),
    Hang,
    Oversized,
}

async fn spawn(routes: Vec<(&'static str, Answer)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let routes = Arc::clone(&routes);
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: Request<Incoming>| {
                let routes = Arc::clone(&routes);
                async move {
                    let path = req.uri().path().to_string();
                    let answer = routes
                        .iter()
                        .find(|(p, _)| path.ends_with(p))
                        .map(|(_, a)| a.clone())
                        .unwrap_or(Answer::Json(StatusCode::NOT_FOUND, "{}".to_string()));
                    let resp = match answer {
                        Answer::Json(status, body) => Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from(body)))
                            .unwrap(),
                        Answer::Hang => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            Response::new(Full::new(Bytes::from("{}")))
                        }
                        Answer::Oversized => Response::builder()
                            .status(StatusCode::OK)
                            .body(Full::new(Bytes::from(vec![
                                b'1';
                                MAX_RESPONSE_BODY_BYTES + 1
                            ])))
                            .unwrap(),
                    };
                    Ok::<_, Infallible>(resp)
                }
            });
            tokio::spawn(async move {
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    addr
}

fn http(timeout_ms: u64) -> FeedHttp {
    FeedHttp::new_plain_for_tests(FeedHttpConfig {
        connect_timeout: Duration::from_millis(timeout_ms),
        request_timeout: Duration::from_millis(timeout_ms),
    })
    .unwrap()
}

fn nonkyc(addr: SocketAddr, market: &str) -> NonKycFeed {
    NonKycFeed {
        http: http(500),
        market: NonKycMarket {
            base_url: format!("http://{addr}/api/v2"),
            market: market.to_string(),
        },
    }
}

#[tokio::test]
async fn nonkyc_valid_document_yields_the_market_price_at_its_update_instant() {
    let addr = spawn(vec![(
        "/market/getbysymbol/GLC_USDT",
        Answer::Json(StatusCode::OK, GLC_DOC.into()),
    )])
    .await;
    let sample = nonkyc(addr, "GLC_USDT").fetch(1_800_000_000).await.unwrap();
    assert_eq!(sample.price_e12, 156_705_000);
    assert_eq!(sample.feed_at, 1_789_429_924);
}

#[tokio::test]
async fn nonkyc_every_failure_class_yields_no_sample() {
    type Case = (&'static str, Answer, fn(&FeedError) -> bool);
    let cases: Vec<Case> = vec![
        (
            "malformed",
            Answer::Json(StatusCode::OK, "not json".into()),
            |e| matches!(e, FeedError::Malformed(_)),
        ),
        (
            "empty object",
            Answer::Json(StatusCode::OK, "{}".into()),
            |e| matches!(e, FeedError::Malformed(_)),
        ),
        (
            "http 500",
            Answer::Json(StatusCode::INTERNAL_SERVER_ERROR, GLC_DOC.into()),
            |e| matches!(e, FeedError::Status { status: 500 }),
        ),
        (
            "http 404",
            Answer::Json(StatusCode::NOT_FOUND, "{}".into()),
            |e| matches!(e, FeedError::Status { status: 404 }),
        ),
        (
            "redirect",
            Answer::Json(StatusCode::FOUND, "".into()),
            |e| matches!(e, FeedError::Status { status: 302 }),
        ),
        (
            "zero",
            Answer::Json(StatusCode::OK, GLC_DOC.replace("\"0.000156705\"", "\"0\"")),
            |e| matches!(e, FeedError::Price(_)),
        ),
        (
            "negative",
            Answer::Json(
                StatusCode::OK,
                GLC_DOC.replace("\"0.000156705\"", "\"-0.1\""),
            ),
            |e| matches!(e, FeedError::Price(_)),
        ),
        (
            "non-numeric",
            Answer::Json(
                StatusCode::OK,
                GLC_DOC.replace("\"0.000156705\"", "\"n/a\""),
            ),
            |e| matches!(e, FeedError::Price(_)),
        ),
        (
            "overflow",
            Answer::Json(
                StatusCode::OK,
                GLC_DOC.replace("\"0.000156705\"", "\"99999999999\""),
            ),
            |e| matches!(e, FeedError::Price(_)),
        ),
        (
            "paused",
            Answer::Json(
                StatusCode::OK,
                GLC_DOC.replace("\"isPaused\":false", "\"isPaused\":true"),
            ),
            |e| matches!(e, FeedError::MarketUnusable(_)),
        ),
        (
            "wrong market",
            Answer::Json(StatusCode::OK, ETH_DOC.into()),
            |e| matches!(e, FeedError::Malformed(_)),
        ),
        ("oversized", Answer::Oversized, |e| {
            matches!(e, FeedError::Malformed(_))
        }),
        ("timeout", Answer::Hang, |e| matches!(e, FeedError::Timeout)),
    ];
    for (name, answer, check) in cases {
        let addr = spawn(vec![("/market/getbysymbol/GLC_USDT", answer)]).await;
        let err = nonkyc(addr, "GLC_USDT").fetch(0).await.unwrap_err();
        assert!(check(&err), "{name}: {err:?}");
    }
    // Unreachable host.
    let feed = NonKycFeed {
        http: http(300),
        market: NonKycMarket {
            base_url: "http://127.0.0.1:9/api/v2".to_string(),
            market: "GLC_USDT".to_string(),
        },
    };
    assert!(matches!(
        feed.fetch(0).await.unwrap_err(),
        FeedError::Transport(_)
    ));
}

#[tokio::test]
async fn a_stale_market_timestamp_is_recorded_as_stale_never_as_fresh() {
    // The document's updatedAt IS the sample's feed_at: a book evaluated
    // later than the staleness bound reports the rail stale, whatever the
    // receipt time was.
    let addr = spawn(vec![(
        "/market/getbysymbol/GLC_USDT",
        Answer::Json(StatusCode::OK, GLC_DOC.into()),
    )])
    .await;
    let book = LiveBook::new(LiveRateConfig {
        price_window_secs: 360,
        price_staleness_secs: 120,
        rate_band_bps: 2_500,
    });
    poll_one(&book, &nonkyc(addr, "GLC_USDT"), 1_789_430_000).await;
    let snap = &book.snapshots(1_789_430_000 + 300)[0];
    assert_eq!(snap.chain, Chain::Goldcoin);
    assert_eq!(snap.newest_feed_at, Some(1_789_429_924));
    assert_eq!(snap.status, crate::bridge_rate::live::RailStatus::Stale);
}

#[tokio::test]
async fn jupiter_valid_document_yields_the_usd_price_stamped_at_receipt() {
    let addr = spawn(vec![(
        "/price/v3",
        Answer::Json(StatusCode::OK, JUP_DOC.into()),
    )])
    .await;
    let feed = JupiterFeed {
        http: http(500),
        base_url: format!("http://{addr}/price/v3"),
        mint: MINT.to_string(),
    };
    let sample = feed.fetch(1_800_000_000).await.unwrap();
    assert_eq!(sample.price_e12, 44_367_007);
    assert_eq!(
        sample.feed_at, 1_800_000_000,
        "no source timestamp: receipt time"
    );
    assert_eq!(sample.observed_at, 1_800_000_000);
    // A document without this mint is a refusal, not a zero.
    let addr = spawn(vec![(
        "/price/v3",
        Answer::Json(StatusCode::OK, "{}".into()),
    )])
    .await;
    let feed = JupiterFeed {
        http: http(500),
        base_url: format!("http://{addr}/price/v3"),
        mint: MINT.to_string(),
    };
    assert!(matches!(
        feed.fetch(0).await.unwrap_err(),
        FeedError::Malformed(_)
    ));
}

#[tokio::test]
async fn the_poller_records_a_sample_or_a_failure_and_never_a_stand_in() {
    let addr = spawn(vec![(
        "/market/getbysymbol/GLC_USDT",
        Answer::Json(StatusCode::INTERNAL_SERVER_ERROR, "{}".into()),
    )])
    .await;
    let book = LiveBook::new(LiveRateConfig {
        price_window_secs: 360,
        price_staleness_secs: 120,
        rate_band_bps: 2_500,
    });
    poll_one(&book, &nonkyc(addr, "GLC_USDT"), 1_000).await;
    let snap = &book.snapshots(1_000)[0];
    assert_eq!(snap.sample_count, 0);
    assert_eq!(snap.last_error.as_deref(), Some("HTTP 500"));
    assert_eq!(snap.last_error_at, Some(1_000));
    let addr = spawn(vec![(
        "/market/getbysymbol/GLC_USDT",
        Answer::Json(StatusCode::OK, GLC_DOC.into()),
    )])
    .await;
    poll_one(&book, &nonkyc(addr, "GLC_USDT"), 1_789_429_930).await;
    let snap = &book.snapshots(1_789_429_930)[0];
    assert_eq!(snap.sample_count, 1);
    assert_eq!(snap.newest_feed_at, Some(1_789_429_924));
}

// ---- Uniswap v4 ---------------------------------------------------------

/// A scripted EVM RPC: `getSlot0`/`getLiquidity` answers by selector, a
/// fixed head.
struct ScriptedEvm {
    sqrt_price_x96: u128,
    liquidity: u128,
    head_timestamp: u64,
    calls: Mutex<Vec<String>>,
}

fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

impl EvmRpc for ScriptedEvm {
    async fn chain_id(&self) -> Result<EvmChainId, EvmRpcError> {
        Ok(EvmChainId::new(4663).unwrap())
    }
    async fn block_number(&self) -> Result<u64, EvmRpcError> {
        Ok(100)
    }
    async fn block_by_number(&self, number: u64) -> Result<Option<EvmBlockRef>, EvmRpcError> {
        Ok(Some(EvmBlockRef {
            number,
            hash: EvmBlockHash::from_bytes([1; 32]),
            parent_hash: EvmBlockHash::from_bytes([0; 32]),
            timestamp: self.head_timestamp,
        }))
    }
    async fn logs(&self, _filter: &EvmLogFilter) -> Result<Vec<EvmRawLog>, EvmRpcError> {
        Ok(Vec::new())
    }
}

impl EvmCallRpc for ScriptedEvm {
    async fn call(&self, call: &EvmCall, _block: EvmBlockTag) -> Result<Vec<u8>, EvmRpcError> {
        let selector = &call.data[..4];
        self.calls.lock().unwrap().push(format!("{selector:02x?}"));
        if selector == crate::evm::abi::selector("getSlot0(bytes32)") {
            let mut out = Vec::new();
            out.extend_from_slice(&word_u128(self.sqrt_price_x96));
            out.extend_from_slice(&word_u128(180_765));
            out.extend_from_slice(&word_u128(0));
            out.extend_from_slice(&word_u128(0));
            return Ok(out);
        }
        if selector == crate::evm::abi::selector("getLiquidity(bytes32)") {
            return Ok(word_u128(self.liquidity).to_vec());
        }
        Err(EvmRpcError::Method {
            code: -32000,
            message: "unknown selector".to_string(),
        })
    }
    async fn code_at(
        &self,
        _address: EvmAddress,
        _block: EvmBlockTag,
    ) -> Result<Vec<u8>, EvmRpcError> {
        Ok(vec![0x60])
    }
}

fn pool() -> UniswapV4Pool {
    UniswapV4Pool {
        state_view: "0xf3334192d15450cdd385c8b70e03f9a6bd9e673b"
            .parse()
            .unwrap(),
        pool_id: [0x70; 32],
        glc_is_currency1: true,
        currency0_decimals: 18,
        currency1_decimals: 18,
    }
}

#[tokio::test]
async fn uniswap_v4_derives_glc_usd_from_the_pool_state_and_the_eth_market() {
    let addr = spawn(vec![(
        "/market/getbysymbol/ETH_USDT",
        Answer::Json(StatusCode::OK, ETH_DOC.into()),
    )])
    .await;
    let feed = UniswapV4Feed {
        rpc: ScriptedEvm {
            sqrt_price_x96: 666_733_903_468_254_117_567_079_170_768_462,
            liquidity: 29_277_002_188_455_995_766_012,
            head_timestamp: 1_789_429_950,
            calls: Mutex::new(Vec::new()),
        },
        pool: pool(),
        http: http(500),
        eth_usd: NonKycMarket {
            base_url: format!("http://{addr}/api/v2"),
            market: "ETH_USDT".to_string(),
        },
    };
    let sample = feed.fetch(1_800_000_000).await.unwrap();
    assert!(
        (35_000_000..36_500_000).contains(&sample.price_e12),
        "{}",
        sample.price_e12
    );
    // feed_at is the OLDER of the head block and the ETH market update.
    assert_eq!(sample.feed_at, 1_789_429_922);
    assert_eq!(
        feed.rpc.calls.lock().unwrap().len(),
        2,
        "getSlot0 and getLiquidity"
    );
    assert!(feed.describe().contains("uniswap v4 pool 0x7070"));
}

#[tokio::test]
async fn uniswap_v4_refuses_an_empty_pool_a_zero_price_and_a_failed_eth_leg() {
    let addr = spawn(vec![(
        "/market/getbysymbol/ETH_USDT",
        Answer::Json(StatusCode::OK, ETH_DOC.into()),
    )])
    .await;
    let mk = |sqrt: u128, liq: u128, eth_addr: SocketAddr| UniswapV4Feed {
        rpc: ScriptedEvm {
            sqrt_price_x96: sqrt,
            liquidity: liq,
            head_timestamp: 1_789_429_950,
            calls: Mutex::new(Vec::new()),
        },
        pool: pool(),
        http: http(500),
        eth_usd: NonKycMarket {
            base_url: format!("http://{eth_addr}/api/v2"),
            market: "ETH_USDT".to_string(),
        },
    };
    assert!(matches!(
        mk(666_733_903_468_254_117_567_079_170_768_462, 0, addr)
            .fetch(0)
            .await
            .unwrap_err(),
        FeedError::MarketUnusable(_)
    ));
    assert!(matches!(
        mk(0, 1, addr).fetch(0).await.unwrap_err(),
        FeedError::Malformed(_)
    ));
    let dead = spawn(vec![(
        "/market/getbysymbol/ETH_USDT",
        Answer::Json(StatusCode::SERVICE_UNAVAILABLE, "{}".into()),
    )])
    .await;
    assert!(matches!(
        mk(666_733_903_468_254_117_567_079_170_768_462, 1, dead)
            .fetch(0)
            .await
            .unwrap_err(),
        FeedError::Status { status: 503 }
    ));
}

#[test]
fn no_feed_client_sends_a_credential() {
    // The one header any feed sets is `accept`; nothing sends an API key
    // or an Authorization header, and nothing reads one from config.
    for (name, source) in [
        ("feeds.rs", include_str!("../feeds.rs")),
        ("nonkyc.rs", include_str!("nonkyc.rs")),
        ("jupiter.rs", include_str!("jupiter.rs")),
        ("uniswap_v4.rs", include_str!("uniswap_v4.rs")),
    ] {
        let headers: Vec<&str> = source
            .match_indices(".header(")
            .map(|(i, _)| &source[i..(i + 40).min(source.len())])
            .collect();
        assert!(
            headers.iter().all(|h| h.contains("\"accept\"")),
            "{name} sets a header other than accept: {headers:?}"
        );
        assert!(!source.contains("bearer_auth"), "{name}");
        assert!(!source.contains("basic_auth"), "{name}");
    }
}
