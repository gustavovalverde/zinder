//! External market-price transport for the Cipherscan compatibility service.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 16 * 1_024;
const FRESH_CACHE_TTL: Duration = Duration::from_mins(1);
const MAX_STALE_CACHE_AGE: Duration = Duration::from_mins(15);
const MAX_HISTORICAL_CACHE_ENTRIES: usize = 1_024;
const HISTORICAL_DATE_PLACEHOLDER: &str = "{date}";

/// Reusable client for the external ZEC/USD market-price endpoints.
#[derive(Clone)]
pub(crate) struct MarketPriceClient {
    client: Client,
    endpoint: Url,
    historical_endpoint_template: String,
    cache: Arc<RwLock<Option<CachedMarketPrice>>>,
    historical_cache: Arc<Mutex<HistoricalPriceCache>>,
    refresh: Arc<Mutex<()>>,
    historical_refresh: Arc<Mutex<()>>,
    fresh_cache_ttl: Duration,
    max_stale_cache_age: Duration,
}

/// Exact successful body of the legacy Cipherscan `/api/price` route.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct CurrentMarketPrice {
    pub(crate) price: f64,
    #[serde(rename = "change24h")]
    pub(crate) change_24h: f64,
    pub(crate) timestamp: u64,
}

/// Exact successful body of the legacy Cipherscan historical-price route.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct HistoricalMarketPrice {
    pub(crate) date: String,
    pub(crate) price_usd: f64,
    pub(crate) exact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actual_date: Option<String>,
}

/// Typed outcome of a successful historical-price provider request.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HistoricalMarketPriceResult {
    Price(HistoricalMarketPrice),
    NoPrice,
}

#[derive(Clone)]
struct CachedMarketPrice {
    body: CurrentMarketPrice,
    fetched_at: Instant,
}

#[derive(Default)]
struct HistoricalPriceCache {
    prices_by_requested_date: HashMap<String, HistoricalMarketPrice>,
    insertion_order: VecDeque<String>,
}

impl HistoricalPriceCache {
    fn get(&self, requested_date: &str) -> Option<HistoricalMarketPrice> {
        self.prices_by_requested_date.get(requested_date).cloned()
    }

    fn insert(&mut self, requested_date: String, price: HistoricalMarketPrice) {
        if self.prices_by_requested_date.contains_key(&requested_date) {
            return;
        }

        if self.prices_by_requested_date.len() == MAX_HISTORICAL_CACHE_ENTRIES
            && let Some(oldest_requested_date) = self.insertion_order.pop_front()
        {
            self.prices_by_requested_date.remove(&oldest_requested_date);
        }
        self.insertion_order.push_back(requested_date.clone());
        self.prices_by_requested_date.insert(requested_date, price);
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamPriceResponse {
    zcash: UpstreamZcashPrice,
}

#[derive(Debug, Deserialize)]
struct UpstreamZcashPrice {
    usd: f64,
    usd_24h_change: f64,
}

#[derive(Debug, Deserialize)]
struct HistoricalUpstreamResponse {
    market_data: Option<HistoricalMarketData>,
}

#[derive(Debug, Deserialize)]
struct HistoricalMarketData {
    current_price: Option<HistoricalCurrentPrice>,
}

#[derive(Debug, Deserialize)]
struct HistoricalCurrentPrice {
    usd: Option<f64>,
}

/// Failure while constructing the external market-price transport.
#[derive(Debug, Error)]
pub enum MarketPriceInitializationError {
    /// The historical endpoint must be a valid URL with one date substitution point.
    #[error(
        "historical price endpoint template must be a valid URL containing exactly one {{date}} placeholder"
    )]
    InvalidHistoricalEndpointTemplate,
    /// The reusable HTTP client could not be constructed.
    #[error("price HTTP client could not be initialized: {0}")]
    HttpClient(#[from] reqwest::Error),
}

/// Failure from the external market-price transport.
#[derive(Debug, Error)]
pub(crate) enum MarketPriceError {
    /// The price service returned an HTTP response outside the 2xx range.
    #[error("price upstream returned HTTP {0}")]
    UpstreamStatus(StatusCode),
    /// Sending the request or reading its body failed.
    #[error("price transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// The response body exceeded the configured transport bound.
    #[error("price response exceeded {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    /// The response body was not valid JSON for the expected upstream shape.
    #[error("price response could not be parsed: {0}")]
    Parse(#[from] serde_json::Error),
    /// The response carried a syntactically valid but unacceptable price value.
    #[error("price response contained invalid field: {0}")]
    Malformed(&'static str),
    /// The local clock could not produce the legacy millisecond timestamp.
    #[error("current timestamp is outside the supported millisecond range")]
    TimestampOutOfRange,
}

impl MarketPriceClient {
    /// Builds a reusable client for both market-price endpoints.
    pub(crate) fn new(
        endpoint: Url,
        historical_endpoint_template: impl Into<String>,
    ) -> Result<Self, MarketPriceInitializationError> {
        Self::with_cache_policy(
            endpoint,
            historical_endpoint_template,
            FRESH_CACHE_TTL,
            MAX_STALE_CACHE_AGE,
        )
    }

    fn with_cache_policy(
        endpoint: Url,
        historical_endpoint_template: impl Into<String>,
        fresh_cache_ttl: Duration,
        max_stale_cache_age: Duration,
    ) -> Result<Self, MarketPriceInitializationError> {
        let historical_endpoint_template = historical_endpoint_template.into();
        validate_historical_endpoint_template(&historical_endpoint_template)?;
        let client = Client::builder()
            .user_agent(concat!(
                "zinder-compat-cipherscan/",
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            endpoint,
            historical_endpoint_template,
            cache: Arc::new(RwLock::new(None)),
            historical_cache: Arc::new(Mutex::new(HistoricalPriceCache::default())),
            refresh: Arc::new(Mutex::new(())),
            historical_refresh: Arc::new(Mutex::new(())),
            fresh_cache_ttl,
            max_stale_cache_age,
        })
    }

    /// Looks up one completed-day historical price without persistent Zinder state.
    ///
    /// `CoinGecko`'s public historical endpoint covers only the past 365 days. The
    /// caller owns range validation because this transport intentionally performs
    /// no date arithmetic.
    pub(crate) async fn historical_price(
        &self,
        requested_date: &str,
    ) -> Result<HistoricalMarketPriceResult, MarketPriceError> {
        if let Some(price) = self.cached_historical_price(requested_date).await {
            return Ok(HistoricalMarketPriceResult::Price(price));
        }

        let _refresh_guard = self.historical_refresh.lock().await;
        if let Some(price) = self.cached_historical_price(requested_date).await {
            return Ok(HistoricalMarketPriceResult::Price(price));
        }

        let endpoint = self
            .historical_endpoint_template
            .replace(HISTORICAL_DATE_PLACEHOLDER, requested_date);
        let mut response = self.client.get(endpoint).send().await?;
        if !response.status().is_success() {
            return Err(MarketPriceError::UpstreamStatus(response.status()));
        }

        let body = read_bounded_body(&mut response).await?;
        let upstream: HistoricalUpstreamResponse = serde_json::from_slice(&body)?;
        let Some(price_usd) = upstream
            .market_data
            .and_then(|market_data| market_data.current_price)
            .and_then(|current_price| current_price.usd)
        else {
            return Ok(HistoricalMarketPriceResult::NoPrice);
        };
        if !price_usd.is_finite() || price_usd <= 0.0 {
            return Err(MarketPriceError::Malformed("market_data.current_price.usd"));
        }

        let rounded_price_usd = (price_usd * 10_000.0).round() / 10_000.0;
        if !rounded_price_usd.is_finite() || rounded_price_usd <= 0.0 {
            return Err(MarketPriceError::Malformed("market_data.current_price.usd"));
        }
        let price = HistoricalMarketPrice {
            date: requested_date.to_owned(),
            price_usd: rounded_price_usd,
            exact: true,
            actual_date: None,
        };
        self.historical_cache
            .lock()
            .await
            .insert(requested_date.to_owned(), price.clone());
        Ok(HistoricalMarketPriceResult::Price(price))
    }

    async fn cached_historical_price(&self, requested_date: &str) -> Option<HistoricalMarketPrice> {
        let cache = self.historical_cache.lock().await;
        cache.get(requested_date)
    }

    /// Returns a fresh price, or an exact cached legacy body during a bounded outage.
    pub(crate) async fn current_price(&self) -> Result<CurrentMarketPrice, MarketPriceError> {
        if let Some(body) = self.cached_body_younger_than(self.fresh_cache_ttl).await {
            return Ok(body);
        }

        let _refresh_guard = self.refresh.lock().await;
        if let Some(body) = self.cached_body_younger_than(self.fresh_cache_ttl).await {
            return Ok(body);
        }

        match self.fetch_current_price().await {
            Ok(body) => {
                *self.cache.write().await = Some(CachedMarketPrice {
                    body: body.clone(),
                    fetched_at: Instant::now(),
                });
                Ok(body)
            }
            Err(error) => self
                .cached_body_younger_than(self.max_stale_cache_age)
                .await
                .ok_or(error),
        }
    }

    async fn cached_body_younger_than(&self, maximum_age: Duration) -> Option<CurrentMarketPrice> {
        let cache = self.cache.read().await;
        cache
            .as_ref()
            .filter(|cached| cached.fetched_at.elapsed() < maximum_age)
            .map(|cached| cached.body.clone())
    }

    async fn fetch_current_price(&self) -> Result<CurrentMarketPrice, MarketPriceError> {
        let mut response = self.client.get(self.endpoint.clone()).send().await?;
        if !response.status().is_success() {
            return Err(MarketPriceError::UpstreamStatus(response.status()));
        }

        let body = read_bounded_body(&mut response).await?;
        let upstream: UpstreamPriceResponse = serde_json::from_slice(&body)?;
        if !upstream.zcash.usd.is_finite() || upstream.zcash.usd <= 0.0 {
            return Err(MarketPriceError::Malformed("zcash.usd"));
        }
        if !upstream.zcash.usd_24h_change.is_finite() {
            return Err(MarketPriceError::Malformed("zcash.usd_24h_change"));
        }

        Ok(CurrentMarketPrice {
            price: upstream.zcash.usd,
            change_24h: upstream.zcash.usd_24h_change,
            timestamp: current_unix_millis()?,
        })
    }
}

pub(super) fn validate_historical_endpoint_template(
    historical_endpoint_template: &str,
) -> Result<(), MarketPriceInitializationError> {
    let parsed_endpoint = Url::parse(
        &historical_endpoint_template.replace(HISTORICAL_DATE_PLACEHOLDER, "01-01-2000"),
    );
    if historical_endpoint_template
        .match_indices(HISTORICAL_DATE_PLACEHOLDER)
        .count()
        != 1
        || !parsed_endpoint
            .as_ref()
            .is_ok_and(|endpoint| matches!(endpoint.scheme(), "http" | "https"))
    {
        return Err(MarketPriceInitializationError::InvalidHistoricalEndpointTemplate);
    }
    Ok(())
}

async fn read_bounded_body(response: &mut reqwest::Response) -> Result<Vec<u8>, MarketPriceError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(MarketPriceError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn current_unix_millis() -> Result<u64, MarketPriceError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MarketPriceError::TimestampOutOfRange)?;
    u64::try_from(elapsed.as_millis()).map_err(|_| MarketPriceError::TimestampOutOfRange)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        Router,
        body::{Body, Bytes},
        extract::State,
        http::Response,
        routing::get,
    };
    use serde_json::json;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[derive(Clone)]
    struct MockState {
        responses: Arc<Mutex<VecDeque<MockResponse>>>,
        request_count: Arc<AtomicUsize>,
    }

    enum MockResponse {
        Json(&'static str),
        Status(StatusCode),
        Chunked(Vec<Bytes>),
    }

    struct MockServer {
        endpoint: Url,
        request_count: Arc<AtomicUsize>,
        task: JoinHandle<std::io::Result<()>>,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl MockServer {
        async fn start(responses: impl IntoIterator<Item = MockResponse>) -> TestResult<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let request_count = Arc::new(AtomicUsize::new(0));
            let state = MockState {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                request_count: request_count.clone(),
            };
            let app = Router::new()
                .route("/price", get(mock_price))
                .route("/history", get(mock_price))
                .with_state(state);
            let task = tokio::spawn(async move { axum::serve(listener, app).await });
            Ok(Self {
                endpoint: format!("http://{address}/price").parse()?,
                request_count,
                task,
            })
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }

        fn historical_endpoint_template(&self) -> String {
            self.endpoint
                .as_str()
                .replace("/price", "/history?date={date}")
        }
    }

    async fn mock_price(State(state): State<MockState>) -> Response<Body> {
        state.request_count.fetch_add(1, Ordering::SeqCst);
        let response = state.responses.lock().await.pop_front();
        match response {
            Some(MockResponse::Json(body)) => Response::new(Body::from(body)),
            Some(MockResponse::Status(status)) => Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty())),
            Some(MockResponse::Chunked(chunks)) => Response::new(Body::from_stream(
                tokio_stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)),
            )),
            None => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty())),
        }
    }

    #[tokio::test]
    async fn returns_exact_legacy_body_with_millisecond_timestamp() -> TestResult {
        let server = MockServer::start([MockResponse::Json(
            r#"{"zcash":{"usd":35.42,"usd_24h_change":-2.15}}"#,
        )])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let body = client.current_price().await?;
        let json = serde_json::to_value(body)?;

        assert_eq!(json["price"], json!(35.42));
        assert_eq!(json["change24h"], json!(-2.15));
        assert!(
            json["timestamp"]
                .as_u64()
                .is_some_and(|value| value > 1_000_000_000_000)
        );
        assert_eq!(json.as_object().map(serde_json::Map::len), Some(3));
        Ok(())
    }

    #[tokio::test]
    async fn coalesces_concurrent_refreshes() -> TestResult {
        let server = MockServer::start([MockResponse::Json(
            r#"{"zcash":{"usd":40.0,"usd_24h_change":1.5}}"#,
        )])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let (first, second, third) = tokio::join!(
            client.current_price(),
            client.current_price(),
            client.current_price()
        );
        let first = first?;
        let second = second?;
        let third = third?;

        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(server.request_count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn returns_exact_stale_body_when_refresh_fails() -> TestResult {
        let server = MockServer::start([
            MockResponse::Json(r#"{"zcash":{"usd":50.0,"usd_24h_change":-0.5}}"#),
            MockResponse::Status(StatusCode::BAD_GATEWAY),
        ])
        .await?;
        let client = MarketPriceClient::with_cache_policy(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
            Duration::ZERO,
            Duration::from_mins(1),
        )?;

        let initial = client.current_price().await?;
        let stale = client.current_price().await?;

        assert_eq!(stale, initial);
        assert_eq!(server.request_count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_cached_body_older_than_stale_limit() -> TestResult {
        let server = MockServer::start([
            MockResponse::Json(r#"{"zcash":{"usd":50.0,"usd_24h_change":-0.5}}"#),
            MockResponse::Status(StatusCode::BAD_GATEWAY),
        ])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;
        client.current_price().await?;
        let stale_fetched_at = Instant::now()
            .checked_sub(Duration::from_mins(16))
            .ok_or("test instant underflow")?;
        client
            .cache
            .write()
            .await
            .as_mut()
            .ok_or("missing cached price")?
            .fetched_at = stale_fetched_at;

        let error = client.current_price().await.err().ok_or("missing error")?;

        assert!(matches!(
            error,
            MarketPriceError::UpstreamStatus(StatusCode::BAD_GATEWAY)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn distinguishes_upstream_status_from_malformed_failures() -> TestResult {
        let status_server =
            MockServer::start([MockResponse::Status(StatusCode::TOO_MANY_REQUESTS)]).await?;
        let status_client = MarketPriceClient::new(
            status_server.endpoint.clone(),
            status_server.historical_endpoint_template(),
        )?;
        let status_error = status_client
            .current_price()
            .await
            .err()
            .ok_or("missing error")?;
        assert!(matches!(
            status_error,
            MarketPriceError::UpstreamStatus(StatusCode::TOO_MANY_REQUESTS)
        ));

        let malformed_server = MockServer::start([MockResponse::Json(
            r#"{"zcash":{"usd":0.0,"usd_24h_change":1.0}}"#,
        )])
        .await?;
        let malformed_client = MarketPriceClient::new(
            malformed_server.endpoint.clone(),
            malformed_server.historical_endpoint_template(),
        )?;
        let malformed_error = malformed_client
            .current_price()
            .await
            .err()
            .ok_or("missing error")?;
        assert!(matches!(
            malformed_error,
            MarketPriceError::Malformed("zcash.usd")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn caps_chunked_response_bodies() -> TestResult {
        let server = MockServer::start([MockResponse::Chunked(vec![
            Bytes::from(vec![b'a'; MAX_RESPONSE_BYTES]),
            Bytes::from_static(b"x"),
        ])])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let error = client.current_price().await.err().ok_or("missing error")?;

        assert!(matches!(error, MarketPriceError::ResponseTooLarge));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_non_finite_change() -> TestResult {
        let server = MockServer::start([MockResponse::Json(
            r#"{"zcash":{"usd":10.0,"usd_24h_change":1e400}}"#,
        )])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let error = client.current_price().await.err().ok_or("missing error")?;

        assert!(matches!(error, MarketPriceError::Parse(_)));
        Ok(())
    }

    #[tokio::test]
    async fn returns_exact_historical_body_with_four_decimal_rounding() -> TestResult {
        let server = MockServer::start([MockResponse::Json(
            r#"{"market_data":{"current_price":{"usd":35.42426}}}"#,
        )])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let HistoricalMarketPriceResult::Price(body) =
            client.historical_price("12-07-2026").await?
        else {
            return Err("missing historical price".into());
        };
        let json = serde_json::to_value(body)?;

        assert_eq!(json["date"], json!("12-07-2026"));
        assert_eq!(json["price_usd"], json!(35.4243));
        assert_eq!(json["exact"], json!(true));
        assert_eq!(json.as_object().map(serde_json::Map::len), Some(3));
        Ok(())
    }

    #[tokio::test]
    async fn caches_successful_historical_prices_by_requested_date() -> TestResult {
        let server = MockServer::start([MockResponse::Json(
            r#"{"market_data":{"current_price":{"usd":40.0}}}"#,
        )])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let (first, second, third) = tokio::join!(
            client.historical_price("11-07-2026"),
            client.historical_price("11-07-2026"),
            client.historical_price("11-07-2026")
        );
        let first = first?;
        let second = second?;
        let third = third?;

        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(server.request_count(), 1);
        Ok(())
    }

    #[test]
    fn bounds_historical_cache_and_evicts_oldest_price() {
        let mut cache = HistoricalPriceCache::default();
        for index in 0..=MAX_HISTORICAL_CACHE_ENTRIES {
            let requested_date = format!("date-{index}");
            cache.insert(
                requested_date.clone(),
                HistoricalMarketPrice {
                    date: requested_date,
                    price_usd: 1.0,
                    exact: true,
                    actual_date: None,
                },
            );
        }

        assert_eq!(cache.prices_by_requested_date.len(), 1_024);
        assert!(cache.get("date-0").is_none());
        assert!(cache.get("date-1024").is_some());
    }

    #[tokio::test]
    async fn returns_typed_no_price_without_caching_it() -> TestResult {
        let server = MockServer::start([
            MockResponse::Json(r#"{"market_data":{}}"#),
            MockResponse::Json(r#"{"market_data":{"current_price":{"usd":20.0}}}"#),
        ])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let missing = client.historical_price("10-07-2026").await?;
        let available = client.historical_price("10-07-2026").await?;

        assert_eq!(missing, HistoricalMarketPriceResult::NoPrice);
        assert!(matches!(available, HistoricalMarketPriceResult::Price(_)));
        assert_eq!(server.request_count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn preserves_historical_upstream_status() -> TestResult {
        let server =
            MockServer::start([MockResponse::Status(StatusCode::TOO_MANY_REQUESTS)]).await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let error = client
            .historical_price("09-07-2026")
            .await
            .err()
            .ok_or("missing error")?;

        assert!(matches!(
            error,
            MarketPriceError::UpstreamStatus(StatusCode::TOO_MANY_REQUESTS)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_malformed_historical_response() -> TestResult {
        let server = MockServer::start([MockResponse::Json(r#"{"market_data":42}"#)]).await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let error = client
            .historical_price("08-07-2026")
            .await
            .err()
            .ok_or("missing error")?;

        assert!(matches!(error, MarketPriceError::Parse(_)));
        Ok(())
    }

    #[tokio::test]
    async fn caps_chunked_historical_response_bodies() -> TestResult {
        let server = MockServer::start([MockResponse::Chunked(vec![
            Bytes::from(vec![b'a'; MAX_RESPONSE_BYTES]),
            Bytes::from_static(b"x"),
        ])])
        .await?;
        let client = MarketPriceClient::new(
            server.endpoint.clone(),
            server.historical_endpoint_template(),
        )?;

        let error = client
            .historical_price("07-07-2026")
            .await
            .err()
            .ok_or("missing error")?;

        assert!(matches!(error, MarketPriceError::ResponseTooLarge));
        Ok(())
    }

    #[test]
    fn validates_historical_endpoint_template() -> TestResult {
        let current_endpoint: Url = "http://127.0.0.1/price".parse()?;
        for invalid_template in [
            "http://127.0.0.1/history",
            "http://127.0.0.1/{date}/history?date={date}",
            "not a URL/{date}",
            "ftp://127.0.0.1/history?date={date}",
        ] {
            let error = MarketPriceClient::new(current_endpoint.clone(), invalid_template)
                .err()
                .ok_or("missing initialization error")?;
            assert!(matches!(
                error,
                MarketPriceInitializationError::InvalidHistoricalEndpointTemplate
            ));
        }

        MarketPriceClient::new(
            current_endpoint,
            "https://api.coingecko.com/api/v3/coins/zcash/history?localization=false&date={date}",
        )?;
        Ok(())
    }
}
