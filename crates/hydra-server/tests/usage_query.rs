//! E3 `GET /usage` over **both** metering backends (plan T8).
//!
//! The two things that make this endpoint dangerous are both *silent*: a decoder
//! that reads ClickHouse's quoted `UInt64` as a JSON number and quietly answers
//! `{"requests":0}`, and a reader that answers from a store that has no rows for
//! this node. Neither throws, and both produce a 200. So the assertions here are
//! mostly about what must NOT be returned: never a zero where the store could not
//! be read, never a fabricated `as_of`, never another tenant's numbers.
//!
//! The ClickHouse cases use a real socket answering fixed bytes — an external
//! system boundary, which dev-plan 铁律 2 requires to be a real process-level
//! double rather than an in-process mock (`wiremock`, the same double the auth
//! tests use).

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::auth::sha256_hex_string;
use hydra_core::breaker::BreakerConfig;
use hydra_core::model::Tenant;
use hydra_server::crypto::StaticKeyProvider;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::RateLimiter;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::sink::UsageSink;
use hydra_server::store::ConfigStore;
use hydra_server::tenant_api::TenantApiConfig;
use hydra_server::usage_query::{select, SqliteUsageQuery, UsageQuery};
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use serde_json::Value;

/// These tests never proxy a request; E3 must not touch the sink at all.
struct NoopSink;

impl UsageSink for NoopSink {
    fn record<'a>(
        &'a self,
        _r: hydra_core::model::UsageRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

const TOKEN: &str = "sk-tenant-token-0123456789abcdef";
const OTHER_TOKEN: &str = "sk-other-tenant-token-0123456789";
const T: &str = "t1";

async fn seed_tenant(pool: &sqlx::SqlitePool, id: &str, domain: &str, token: Option<&str>) {
    hydra_server::db::insert_tenant(
        pool,
        &Tenant {
            id: id.into(),
            name: format!("{id}-name"),
            domain: domain.into(),
            auth_url: format!("https://auth.{domain}/verify"),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .await
    .expect("insert tenant");
    if let Some(tok) = token {
        hydra_server::db::set_tenant_access_token_hash(
            pool,
            id,
            Some(&sha256_hex_string(tok.as_bytes())),
        )
        .await
        .expect("set token hash");
    }
}

/// One metering row, written the way the sink writes it: `created_at` is an
/// explicit fixed-width UTC string, never `datetime('now')`.
#[allow(clippy::too_many_arguments)]
async fn insert_usage(
    pool: &sqlx::SqlitePool,
    tenant: &str,
    provider: &str,
    model: &str,
    status: i64,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    cache_hit: Option<i64>,
    created_at: &str,
) {
    sqlx::query(
        "INSERT INTO usage_record (tenant_id, provider_id, model_key, client_api_key, \
         status_code, tokens_in, tokens_out, cache_hit_tokens, latency_ms, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(provider)
    .bind(model)
    .bind("sk-cli***")
    .bind(status)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cache_hit)
    .bind(12i64)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("insert usage row");
}

/// The state a node would build, with the usage capability **injected** exactly
/// as `main` injects it. `None` mirrors a node with no readable store.
async fn build_state(
    pool: &sqlx::SqlitePool,
    cfg: TenantApiConfig,
    usage: Option<Arc<dyn UsageQuery>>,
) -> Arc<AppState> {
    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), kp)
        .await
        .expect("ConfigStore::load");
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker::new"),
    );
    AppState::for_tests_with_usage(
        store,
        auth,
        Arc::new(CircuitBreaker::new(BreakerConfig::new(5))),
        Arc::new(RateLimiter::new()),
        Arc::new(NoopSink) as Arc<dyn UsageSink>,
        ProxyConfig::default(),
        cfg,
        usage,
    )
}

/// A node whose usage store is the local SQLite pool.
async fn build_sqlite_state(pool: &sqlx::SqlitePool) -> Arc<AppState> {
    let usage = Some(Arc::new(SqliteUsageQuery::new(pool.clone())) as Arc<dyn UsageQuery>);
    build_state(pool, TenantApiConfig::default(), usage).await
}

fn start_proxy(state: Arc<AppState>) -> String {
    let port = common::ephemeral_port();
    let listen = format!("127.0.0.1:{port}");
    let app = HydraProxy::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = pingora_proxy::http_proxy_service(&server.configuration, app);
    svc.add_tcp(&listen);
    server.add_service(svc);
    std::thread::spawn(move || {
        server.run_forever();
    });
    format!("http://127.0.0.1:{port}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

/// Retry until the listener accepts; a sleep would be a flake generator.
async fn get_until_ready(
    c: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
) -> reqwest::Response {
    for _ in 0..60 {
        let mut req = c.get(url);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        match req.send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    panic!("proxy never became ready at {url}");
}

async fn get_json(c: &reqwest::Client, url: &str, bearer: &str) -> (u16, Value) {
    let r = get_until_ready(c, url, Some(bearer)).await;
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn usage_url(pool: &sqlx::SqlitePool) -> (String, reqwest::Client) {
    seed_tenant(pool, T, "acme.example", Some(TOKEN)).await;
    let root = start_proxy(build_sqlite_state(pool).await);
    let c = client();
    // Probe once so the listener is up before the measured request.
    let _ = get_until_ready(&c, &format!("{root}/tenant/{T}/api/v1/whoami"), Some(TOKEN)).await;
    (root, c)
}

// ---------------------------------------------------------------------------
// T14 — the capability is chosen from the sink kind, in ONE library function
// ---------------------------------------------------------------------------

#[tokio::test]
async fn select_sqlite_returns_the_sqlite_reader() {
    let pool = common::setup_pool().await;
    let q = select("sqlite", Some(&pool), None).expect("sqlite is selectable");
    assert_eq!(q.source(), "sqlite");
}

/// A `sqlite` node without a pool must NOT fall back to something that answers
/// zero: that is precisely the "well-formed zero" the capability exists to stop.
#[tokio::test]
async fn select_sqlite_without_a_pool_is_an_error() {
    assert!(select("sqlite", None, None).is_err());
}

#[tokio::test]
async fn select_an_unknown_kind_is_an_error() {
    let pool = common::setup_pool().await;
    assert!(select("nonsense", Some(&pool), None).is_err());
    assert!(select("", Some(&pool), None).is_err());
}

/// With `usage-clickhouse` the kind is selectable and reports its own source.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test]
async fn select_clickhouse_returns_the_clickhouse_reader() {
    let q = select("clickhouse", None, Some("http://127.0.0.1:8123"))
        .expect("clickhouse is selectable with the feature");
    assert_eq!(q.source(), "clickhouse");
}

/// The CH arm needs a URL; without one there is nothing to talk to, and an
/// implementation must not silently answer from a different store.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test]
async fn select_clickhouse_without_a_url_is_an_error() {
    assert!(select("clickhouse", None, None).is_err());
}

/// Without the feature, `build_sink` already refuses `HYDRA_USAGE_SINK=clickhouse`
/// at startup, so the kind must not be selectable here either — asserting that
/// keeps the two guards from drifting apart.
#[cfg(not(feature = "usage-clickhouse"))]
#[tokio::test]
async fn select_clickhouse_cannot_be_chosen_without_the_feature() {
    let pool = common::setup_pool().await;
    assert!(
        select("clickhouse", Some(&pool), Some("http://127.0.0.1:8123")).is_err(),
        "a build without usage-clickhouse must not hand out a ClickHouse reader"
    );
}

// ---------------------------------------------------------------------------
// T15 — the SQLite window, field by field, against hand-written SQL
// ---------------------------------------------------------------------------

/// The aggregate must equal what the same window returns when asked directly.
/// This is the assertion that catches a wrong comparison operator (`>` vs `>=`),
/// a wrong bound in the statement, or a `SUM` that leaks NULL.
#[tokio::test]
async fn sqlite_aggregate_matches_hand_written_sql() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, T, "acme.example", Some(TOKEN)).await;
    // Inside the window.
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(100),
        Some(10),
        Some(5),
        "2026-09-16T10:00:00Z",
    )
    .await;
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(200),
        Some(20),
        None,
        "2026-09-16T11:00:00Z",
    )
    .await;
    insert_usage(
        &pool,
        T,
        "p2",
        "claude",
        500,
        Some(50),
        Some(0),
        Some(0),
        "2026-09-16T12:00:00Z",
    )
    .await;
    // Outside: one row before `since`, one exactly at `until` (exclusive).
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(999),
        Some(999),
        None,
        "2026-09-15T23:59:59Z",
    )
    .await;
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(888),
        Some(888),
        None,
        "2026-09-17T00:00:00Z",
    )
    .await;
    // Another tenant, same window: must never be counted.
    insert_usage(
        &pool,
        "t2",
        "p1",
        "gpt-4o",
        200,
        Some(7777),
        Some(7777),
        None,
        "2026-09-16T10:00:00Z",
    )
    .await;

    let q = select("sqlite", Some(&pool), None).expect("select");
    let agg = q
        .aggregate(
            T,
            "2026-09-16T00:00:00Z",
            "2026-09-17T00:00:00Z",
            hydra_server::usage_query::GroupBy::None,
        )
        .await
        .expect("aggregate");

    let handwritten: (i64, i64, i64, i64, i64, Option<String>) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), \
         COALESCE(SUM(cache_hit_tokens),0), \
         COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END),0), MAX(created_at) \
         FROM usage_record WHERE tenant_id = ? AND created_at >= ? AND created_at < ?",
    )
    .bind(T)
    .bind("2026-09-16T00:00:00Z")
    .bind("2026-09-17T00:00:00Z")
    .fetch_one(&pool)
    .await
    .expect("hand-written");

    assert_eq!(agg.totals.requests, handwritten.0 as u64);
    assert_eq!(agg.totals.tokens_in, handwritten.1 as u64);
    assert_eq!(agg.totals.tokens_out, handwritten.2 as u64);
    assert_eq!(agg.totals.cache_hit_tokens, handwritten.3 as u64);
    assert_eq!(agg.totals.errors, handwritten.4 as u64);
    assert_eq!(agg.as_of, handwritten.5);
    // Spell the interesting numbers out too: a query that returned 0 rows for
    // BOTH sides would satisfy the equality above and prove nothing.
    assert_eq!(agg.totals.requests, 3, "3 in-window rows");
    assert_eq!(agg.totals.tokens_in, 350, "100 + 200 + 50");
    assert_eq!(
        agg.totals.cache_hit_tokens, 5,
        "NULL must sum as 0, not poison the SUM"
    );
    assert_eq!(agg.totals.errors, 1, "one 5xx");
    assert_eq!(agg.as_of.as_deref(), Some("2026-09-16T12:00:00Z"));
}

#[tokio::test]
async fn sqlite_group_by_model_matches_hand_written_sql() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, T, "acme.example", Some(TOKEN)).await;
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(100),
        Some(10),
        None,
        "2026-09-16T10:00:00Z",
    )
    .await;
    insert_usage(
        &pool,
        T,
        "p2",
        "gpt-4o",
        500,
        Some(1),
        Some(1),
        None,
        "2026-09-16T11:00:00Z",
    )
    .await;
    insert_usage(
        &pool,
        T,
        "p1",
        "claude",
        200,
        Some(7),
        Some(3),
        Some(2),
        "2026-09-16T12:00:00Z",
    )
    .await;

    let q = select("sqlite", Some(&pool), None).expect("select");
    let agg = q
        .aggregate(
            T,
            "2026-09-16T00:00:00Z",
            "2026-09-17T00:00:00Z",
            hydra_server::usage_query::GroupBy::Model,
        )
        .await
        .expect("aggregate");

    let handwritten: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT model_key, COUNT(*), COALESCE(SUM(tokens_in),0), \
         COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END),0) \
         FROM usage_record WHERE tenant_id = ? AND created_at >= ? AND created_at < ? \
         GROUP BY model_key ORDER BY model_key",
    )
    .bind(T)
    .bind("2026-09-16T00:00:00Z")
    .bind("2026-09-17T00:00:00Z")
    .fetch_all(&pool)
    .await
    .expect("hand-written");

    let rows = hydra_server::usage_query::rows_by_key(&agg);
    assert_eq!(rows.len(), handwritten.len(), "row set must match");
    for (key, requests, tokens_in, errors) in handwritten {
        let got = rows
            .get(&key)
            .unwrap_or_else(|| panic!("missing group {key}"));
        assert_eq!(got.requests, requests as u64, "{key}");
        assert_eq!(got.tokens_in, tokens_in as u64, "{key}");
        assert_eq!(got.errors, errors as u64, "{key}");
    }
    assert_eq!(rows["gpt-4o"].requests, 2);
    assert_eq!(rows["claude"].tokens_in, 7);
}

/// A tenant with no rows in the window gets a real zero *and* `as_of: null` —
/// this is the one place a zero is the correct answer.
#[tokio::test]
async fn sqlite_empty_window_is_a_real_zero_with_no_as_of() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, T, "acme.example", Some(TOKEN)).await;
    let q = select("sqlite", Some(&pool), None).expect("select");
    let agg = q
        .aggregate(
            T,
            "2026-09-16T00:00:00Z",
            "2026-09-17T00:00:00Z",
            hydra_server::usage_query::GroupBy::None,
        )
        .await
        .expect("aggregate");
    assert_eq!(agg.totals.requests, 0);
    assert_eq!(agg.as_of, None);
}

// ---------------------------------------------------------------------------
// T16 — a space-separated `since` is normalised, and the counter-example
// ---------------------------------------------------------------------------

/// The stored column is `2026-09-16T10:00:00Z` while a space-separated bound is
/// `2026-09-16 12:00:00`; at the separator, `'T'` (0x54) > `' '` (0x20), so the
/// RAW form compares *greater* than every row of that day and silently drags the
/// window back to midnight. Both halves are asserted: the endpoint normalises,
/// and the raw form really would overcount.
///
/// The bound is deliberately NOT midnight: at midnight the day digits decide the
/// comparison and the two spellings agree, so a midnight fixture would assert
/// nothing (this test originally used one, and its own premise was false).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_space_separated_since_is_normalised_instead_of_overcounting() {
    let pool = common::setup_pool().await;
    // Before the bound, but on the SAME day as it: the row the raw form wrongly
    // includes.
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(1000),
        Some(0),
        None,
        "2026-09-16T10:00:00Z",
    )
    .await;
    // After the bound.
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(5),
        Some(0),
        None,
        "2026-09-16T14:00:00Z",
    )
    .await;
    // The previous day, which neither form includes.
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(555),
        Some(0),
        None,
        "2026-09-15T23:59:59Z",
    )
    .await;
    let (root, c) = usage_url(&pool).await;

    // (a) The endpoint accepts the historical space-separated form…
    let url = format!(
        "{root}/tenant/{T}/api/v1/usage?since=2026-09-16%2012:00:00&until=2026-09-17T00:00:00Z"
    );
    let (status, v) = get_json(&c, &url, TOKEN).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["since"], "2026-09-16T12:00:00Z",
        "the response must echo the NORMALISED bound: {v}"
    );
    assert_eq!(v["totals"]["requests"], 1, "only the 14:00 row: {v}");
    assert_eq!(v["totals"]["tokens_in"], 5, "{v}");

    // (b) …and the counter-example: the raw form matches the 10:00 row too,
    //     because 'T' > ' ' makes it compare greater than any row of the day.
    let naive: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM usage_record WHERE tenant_id = ? AND created_at >= ? AND created_at < ?",
    )
    .bind(T)
    .bind("2026-09-16 12:00:00")
    .bind("2026-09-17T00:00:00Z")
    .fetch_one(&pool)
    .await
    .expect("naive");
    assert_eq!(
        (naive.0, v["totals"]["requests"].as_i64().unwrap_or(-1)),
        (2, 1),
        "the premise of this test: an un-normalised bound overcounts within the day"
    );
}

/// Epoch seconds and milliseconds are accepted too, and land on the same window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn epoch_bounds_are_accepted_and_normalised() {
    let pool = common::setup_pool().await;
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(5),
        Some(0),
        None,
        "2026-09-16T10:00:00Z",
    )
    .await;
    let (root, c) = usage_url(&pool).await;
    for since in ["1789516800", "1789516800000"] {
        let url =
            format!("{root}/tenant/{T}/api/v1/usage?since={since}&until=2026-09-17T00:00:00Z");
        let (status, v) = get_json(&c, &url, TOKEN).await;
        assert_eq!(status, 200, "since={since}: {v}");
        assert_eq!(v["since"], "2026-09-16T00:00:00Z", "since={since}: {v}");
        assert_eq!(v["totals"]["requests"], 1, "since={since}: {v}");
    }
}

// ---------------------------------------------------------------------------
// T17 — window validation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_over_the_limit_is_rejected_not_scanned() {
    let pool = common::setup_pool().await;
    let (root, c) = usage_url(&pool).await;
    let url = format!(
        "{root}/tenant/{T}/api/v1/usage?since=2026-01-01T00:00:00Z&until=2026-09-17T00:00:00Z"
    );
    let (status, v) = get_json(&c, &url, TOKEN).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "window_too_large", "{v}");
}

/// A window of exactly the limit is allowed; one second more is not. This is the
/// boundary an off-by-one would move.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_of_exactly_the_limit_is_allowed() {
    let pool = common::setup_pool().await;
    let (root, c) = usage_url(&pool).await;
    let at = format!(
        "{root}/tenant/{T}/api/v1/usage?since=2026-08-17T00:00:00Z&until=2026-09-17T00:00:00Z"
    );
    let (status, v) = get_json(&c, &at, TOKEN).await;
    assert_eq!(status, 200, "exactly 31 days must be allowed: {v}");

    let over = format!(
        "{root}/tenant/{T}/api/v1/usage?since=2026-08-16T23:59:59Z&until=2026-09-17T00:00:00Z"
    );
    let (status, v) = get_json(&c, &over, TOKEN).await;
    assert_eq!(status, 400, "one second past the limit: {v}");
    assert_eq!(v["error"]["code"], "window_too_large", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reversed_window_and_a_missing_since_are_rejected() {
    let pool = common::setup_pool().await;
    let (root, c) = usage_url(&pool).await;
    let reversed = format!(
        "{root}/tenant/{T}/api/v1/usage?since=2026-09-17T00:00:00Z&until=2026-09-16T00:00:00Z"
    );
    let (status, v) = get_json(&c, &reversed, TOKEN).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "invalid_since", "{v}");

    let missing = format!("{root}/tenant/{T}/api/v1/usage");
    let (status, v) = get_json(&c, &missing, TOKEN).await;
    assert_eq!(status, 400, "since is required: {v}");
    assert_eq!(v["error"]["code"], "invalid_since", "{v}");

    let junk = format!("{root}/tenant/{T}/api/v1/usage?since=yesterday");
    let (status, v) = get_json(&c, &junk, TOKEN).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "invalid_since", "{v}");
}

/// `group_by` is a whitelist because the column name is interpolated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_group_by_is_rejected_rather_than_interpolated() {
    let pool = common::setup_pool().await;
    let (root, c) = usage_url(&pool).await;
    let url = format!(
        "{root}/tenant/{T}/api/v1/usage?since=2026-09-16T00:00:00Z&group_by=model_key;DROP%20TABLE%20usage_record"
    );
    let (status, v) = get_json(&c, &url, TOKEN).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "invalid_group_by", "{v}");
}

// ---------------------------------------------------------------------------
// T18 — the tenant comes from the token, never from the query string
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_id_query_parameter_cannot_change_whose_usage_is_read() {
    let pool = common::setup_pool().await;
    insert_usage(
        &pool,
        T,
        "p1",
        "gpt-4o",
        200,
        Some(5),
        Some(0),
        None,
        "2026-09-16T10:00:00Z",
    )
    .await;
    insert_usage(
        &pool,
        "t2",
        "p1",
        "gpt-4o",
        200,
        Some(9999),
        Some(0),
        None,
        "2026-09-16T10:00:00Z",
    )
    .await;
    seed_tenant(&pool, T, "acme.example", Some(TOKEN)).await;
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    let root = start_proxy(build_sqlite_state(&pool).await);
    let c = client();
    let url = format!(
        "{root}/tenant/{T}/api/v1/usage?tenant_id=t2&since=2026-09-16T00:00:00Z&until=2026-09-17T00:00:00Z"
    );
    let (status, v) = get_json(&c, &url, TOKEN).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["tenant_id"], T, "the token decides: {v}");
    assert_eq!(v["totals"]["tokens_in"], 5, "not t2's 9999: {v}");
}

// ---------------------------------------------------------------------------
// The response contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_response_carries_its_window_source_and_echoed_bounds() {
    let pool = common::setup_pool().await;
    let (root, c) = usage_url(&pool).await;
    let url = format!(
        "{root}/tenant/{T}/api/v1/usage?since=2026-09-16T00:00:00Z&until=2026-09-17T00:00:00Z&group_by=model"
    );
    let (status, v) = get_json(&c, &url, TOKEN).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["tenant_id"], T, "{v}");
    assert_eq!(v["since"], "2026-09-16T00:00:00Z", "{v}");
    assert_eq!(v["until"], "2026-09-17T00:00:00Z", "{v}");
    assert_eq!(v["as_of"], Value::Null, "no rows ⇒ no as_of: {v}");
    assert_eq!(v["source"], "sqlite", "the reader names itself: {v}");
    assert_eq!(v["group_by"], "model", "{v}");
    for field in [
        "requests",
        "tokens_in",
        "tokens_out",
        "cache_hit_tokens",
        "errors",
    ] {
        assert_eq!(v["totals"][field], 0, "totals.{field} must be present: {v}");
    }
    assert!(v["rows"].is_array(), "{v}");
    // A tenant's own control-plane call must never be billed to it: E3 does not
    // write a usage row.
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM usage_record")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 0, "querying usage must not itself create usage");
}

/// `until` is optional and defaults to now, so the documented "usage after a
/// timestamp" call works without a second parameter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn until_defaults_to_now_when_it_is_omitted() {
    let pool = common::setup_pool().await;
    let (root, c) = usage_url(&pool).await;
    let url = format!("{root}/tenant/{T}/api/v1/usage?since=2026-09-16T00:00:00Z");
    let (status, v) = get_json(&c, &url, TOKEN).await;
    assert_eq!(status, 200, "{v}");
    let until = v["until"].as_str().unwrap_or_default();
    assert!(
        until.len() == 20 && until.ends_with('Z') && until.contains('T'),
        "until must be the canonical form: {until}"
    );
    assert!(until > "2026-09-16T00:00:00Z", "{until}");
}

/// A node with no readable usage store answers 503, never a zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_without_a_usage_store_answers_503_not_zero() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, T, "acme.example", Some(TOKEN)).await;
    let root = start_proxy(build_state(&pool, TenantApiConfig::default(), None).await);
    let c = client();
    let url = format!("{root}/tenant/{T}/api/v1/usage?since=2026-09-16T00:00:00Z");
    let (status, v) = get_json(&c, &url, TOKEN).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["error"]["code"], "usage_store_unavailable", "{v}");
}

// ---------------------------------------------------------------------------
// T14a / T14b / T14c — the ClickHouse transport, over a real socket
// ---------------------------------------------------------------------------

/// Start a process-level ClickHouse double that answers every request with
/// `body`, and return its URL and a handle to the requests it received.
#[cfg(feature = "usage-clickhouse")]
async fn fake_clickhouse(body: &'static str) -> (String, wiremock::MockServer) {
    use wiremock::matchers::{any, method};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    (server.uri(), server)
}

/// A ClickHouse-backed node: the gate reads the local SQLite config, the usage
/// read goes to the double — exactly the edge-node shape of C13.
#[cfg(feature = "usage-clickhouse")]
async fn build_ch_state(pool: &sqlx::SqlitePool, ch_url: &str) -> Arc<AppState> {
    let usage = select("clickhouse", None, Some(ch_url)).expect("clickhouse readable");
    build_state(pool, TenantApiConfig::default(), Some(usage)).await
}

#[cfg(feature = "usage-clickhouse")]
async fn ch_usage(pool: &sqlx::SqlitePool, ch_url: &str, query: &str) -> (u16, Value) {
    seed_tenant(pool, T, "acme.example", Some(TOKEN)).await;
    let root = start_proxy(build_ch_state(pool, ch_url).await);
    let c = client();
    let url = format!("{root}/tenant/{T}/api/v1/usage?{query}");
    get_json(&c, &url, TOKEN).await
}

/// The totals body ClickHouse sends: **every 64-bit integer is a JSON string**
/// (`output_format_json_quote_64bit_integers`), which is the trap.
#[cfg(feature = "usage-clickhouse")]
const CH_QUOTED: &str = r#"{"requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}"#;

#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_quoted_integers_are_read_as_numbers_and_never_as_zero() {
    let pool = common::setup_pool().await;
    let (url, _server) = fake_clickhouse(CH_QUOTED).await;
    let (status, v) = ch_usage(
        &pool,
        &url,
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z",
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["totals"]["requests"], 8, "quoted UInt64 must decode: {v}");
    assert_eq!(v["totals"]["tokens_in"], 247, "{v}");
    assert_eq!(v["source"], "clickhouse", "{v}");
    assert_eq!(v["as_of"], "2026-09-15T07:32:06Z", "{v}");
}

/// The same body with plain JSON numbers must decode identically — the parser
/// accepts both spellings, because which one arrives depends on a server setting.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_plain_integers_decode_the_same_way() {
    let pool = common::setup_pool().await;
    let (url, _server) = fake_clickhouse(
        r#"{"requests":8,"tokens_in":247,"tokens_out":18,"cache_hit_tokens":0,"errors":0,"last_seen":"2026-09-15T07:32:06Z"}"#,
    )
    .await;
    let (status, v) = ch_usage(
        &pool,
        &url,
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z",
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["totals"]["requests"], 8, "{v}");
    assert_eq!(v["totals"]["tokens_in"], 247, "{v}");
}

/// THE assertion of this task: an undecodable counter must become a 503, because
/// reading it as 0 tells the tenant "you used nothing" — a wrong answer a caller
/// cannot detect.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_a_non_numeric_counter_is_a_failure_not_a_zero() {
    let pool = common::setup_pool().await;
    let (url, _server) = fake_clickhouse(
        r#"{"requests":"abc","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}"#,
    )
    .await;
    let before = hydra_server::admin::metrics::tenant_api_usage_query_total(
        "clickhouse",
        "model",
        "decode_error",
    );
    // `group_by=model` on purpose: the metric is process-global and the sibling
    // shape-drift test fails a read with the same `source` and no grouping, so
    // this label combination is what makes the `== 1.0` below an assertion about
    // THIS failure rather than about test scheduling.
    let (status, v) = ch_usage(
        &pool,
        &url,
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z&group_by=model",
    )
    .await;
    assert_ne!(status, 200, "must not answer a well-formed body here: {v}");
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["error"]["code"], "usage_store_unavailable", "{v}");
    assert!(
        v["totals"].is_null(),
        "a failed read must not carry a zeroed totals object: {v}"
    );
    // The tenant cannot tell this apart from an unreachable store, so the metric
    // is the operator's only signal that the store answered something we could
    // not read. Counters are process-global, hence the delta.
    assert_eq!(
        hydra_server::admin::metrics::tenant_api_usage_query_total(
            "clickhouse",
            "model",
            "decode_error"
        ) - before,
        1.0,
        "a decode failure must be attributed as `decode_error`"
    );
}

/// A response missing `last_seen` is a shape drift, not "no records": the
/// aggregate query always projects it.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_a_body_without_last_seen_is_not_read_as_no_records() {
    let pool = common::setup_pool().await;
    let (url, _server) = fake_clickhouse(
        r#"{"requests":"8","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0"}"#,
    )
    .await;
    let (status, v) = ch_usage(
        &pool,
        &url,
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z",
    )
    .await;
    assert_eq!(
        status, 503,
        "shape drift must not read as an empty window: {v}"
    );
}

/// CH-B: with no matching rows ClickHouse answers `last_seen: ""`, which must
/// become `null` — not an empty `as_of` string.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_an_empty_result_set_maps_last_seen_to_null() {
    let pool = common::setup_pool().await;
    let (url, _server) = fake_clickhouse(
        r#"{"requests":"0","tokens_in":"0","tokens_out":"0","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
    )
    .await;
    let (status, v) = ch_usage(
        &pool,
        &url,
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z",
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["as_of"],
        Value::Null,
        "empty string must map to null: {v}"
    );
    assert_eq!(v["totals"]["requests"], 0, "this zero IS the answer: {v}");
}

/// CH-D: a failing query is HTTP 404 with a `Code: N` body, not a 5xx.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_a_query_error_is_a_store_failure() {
    use wiremock::matchers::{any, method};
    use wiremock::{Mock, ResponseTemplate};
    let pool = common::setup_pool().await;
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(any())
        .respond_with(
            ResponseTemplate::new(404).set_body_string(
                "Code: 60. DB::Exception: Table default.usage_record does not exist",
            ),
        )
        .mount(&server)
        .await;
    let (status, v) = ch_usage(
        &pool,
        &server.uri(),
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z",
    )
    .await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["error"]["code"], "usage_store_unavailable", "{v}");
}

/// A 5xx is the same story as the 404: the store exists but did not answer the
/// query. The distinction between "business error" (404 + `Code: N`) and
/// "unavailable" (5xx) matters for the operator's log, not for the tenant.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_a_5xx_is_a_store_failure_too() {
    use wiremock::matchers::{any, method};
    use wiremock::{Mock, ResponseTemplate};
    let pool = common::setup_pool().await;
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(any())
        .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
        .mount(&server)
        .await;
    let (status, v) = ch_usage(
        &pool,
        &server.uri(),
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z",
    )
    .await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["error"]["code"], "usage_store_unavailable", "{v}");
    assert!(
        v["totals"].is_null(),
        "a 5xx must not be reported as zero usage: {v}"
    );
}

/// C13a: an unreachable ClickHouse (here: a closed port) is a 503 with an
/// observation, never a 200 with zeroes and never a panic.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_an_unreachable_store_is_503_not_a_fake_zero() {
    let pool = common::setup_pool().await;
    // A port that nothing listens on: bind, learn the port, drop the listener.
    let port = common::ephemeral_port();
    let (status, v) = ch_usage(
        &pool,
        &format!("http://127.0.0.1:{port}"),
        "since=2026-09-15T00:00:00Z&until=2026-09-16T00:00:00Z",
    )
    .await;
    assert_eq!(status, 503, "unreachable store must not produce a 200: {v}");
    assert_eq!(v["error"]["code"], "usage_store_unavailable", "{v}");
    // `is_null()` and NOT `assert_ne!(v["totals"]["requests"], 0)`: indexing a
    // missing field yields `Null`, and `Null != 0` is true — the weaker form
    // passes whether the body carries a zeroed totals object or no totals at
    // all, so it cannot fail for the reason it claims to test.
    assert!(
        v["totals"].is_null(),
        "a failed read must not carry a zeroed totals object: {v}"
    );
}

/// The bound values must reach ClickHouse as `param_*`, percent-encoded, with
/// the SQL sent as `query=`. A tenant id is attacker-influenced text, so the
/// alternative — interpolating it — is SQL injection into an external database.
///
/// Asserted at the READER, not through the endpoint: the router hands out the
/// tenant id from the path, and a path-encoded id would test the router's
/// decoding rather than the binding. The reader is where the SQL is built.
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clickhouse_binds_its_parameters_instead_of_interpolating_them() {
    use wiremock::matchers::{any, method};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(CH_QUOTED))
        .mount(&server)
        .await;

    // A tenant id built to break out of a string literal, plus a `&` that would
    // truncate the query string if the value were not encoded.
    let evil = "t1' OR 1=1 --&x=1";
    let reader = select("clickhouse", None, Some(&server.uri())).expect("select");
    let agg = reader
        .aggregate(
            evil,
            "2026-09-15T00:00:00Z",
            "2026-09-16T00:00:00Z",
            hydra_server::usage_query::GroupBy::None,
        )
        .await
        .expect("the double always answers 200");
    assert_eq!(agg.totals.requests, 8, "the double's body must decode");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(reqs.len(), 1, "an ungrouped read is exactly one query");
    let target = reqs[0].url.as_str();
    let path_and_query = target.split('?').nth(1).unwrap_or_default();
    assert!(
        path_and_query.contains("query="),
        "the SQL travels in `query=`: {path_and_query}"
    );
    assert!(
        path_and_query.contains("param_t="),
        "the tenant id must be a bound parameter: {path_and_query}"
    );
    assert!(
        !path_and_query.contains("OR 1=1"),
        "an unescaped bound value would be injection: {path_and_query}"
    );
    assert!(
        path_and_query.contains("%27") && path_and_query.contains("%3D"),
        "the bound value must be percent-encoded (a raw `&` would truncate it): {path_and_query}"
    );
    assert!(
        !path_and_query.contains("&x=1"),
        "the `&` inside the tenant id must not survive as a separator: {path_and_query}"
    );
}

// ---------------------------------------------------------------------------
// C13 — live ClickHouse (`--ignored`; the measured ground truth)
// ---------------------------------------------------------------------------

/// Run against the bundled ClickHouse instance and compare with the same query
/// run by hand, i.e. the "集群下可用且与直连 SQL 一致" evidence.
///
/// ```bash
/// CH_URL=http://127.0.0.1:8123 cargo test -p hydra-server \
///   --features server,usage-clickhouse --test usage_query -- --ignored --nocapture
/// ```
#[cfg(feature = "usage-clickhouse")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live ClickHouse (CH_URL, default http://127.0.0.1:8123)"]
async fn live_clickhouse_aggregate_matches_a_hand_run_query() {
    let ch = std::env::var("CH_URL").unwrap_or_else(|_| "http://127.0.0.1:8123".to_string());
    // The tenant that the bundled dataset actually has rows for.
    let tenant = std::env::var("CH_TENANT").unwrap_or_else(|_| "local".to_string());

    // The reader, exactly as `main` would inject it.
    let reader = select("clickhouse", None, Some(&ch)).expect("live CH is selectable");
    let agg = reader
        .aggregate(
            &tenant,
            "2026-09-15T00:00:00Z",
            "2026-09-16T00:00:00Z",
            hydra_server::usage_query::GroupBy::Model,
        )
        .await
        .expect("live ClickHouse read");
    assert_eq!(reader.source(), "clickhouse");

    // The same window, asked directly over HTTP.
    let expected: (i64, i64) = {
        let q = format!(
            "SELECT count() AS c, COALESCE(sum(tokens_in),0) AS t FROM usage_record \
             WHERE tenant_id = '{}' AND created_at >= '2026-09-15T00:00:00Z' \
             AND created_at < '2026-09-16T00:00:00Z' FORMAT JSONEachRow",
            tenant
        );
        let body = reqwest::Client::new()
            .post(format!("{ch}/"))
            .body(q)
            .send()
            .await
            .expect("live CH")
            .text()
            .await
            .expect("body");
        let o: Value = serde_json::from_str(body.trim()).expect("one line");
        (
            o["c"].as_str().unwrap_or("0").parse().unwrap_or(0),
            o["t"].as_str().unwrap_or("0").parse().unwrap_or(0),
        )
    };
    println!("reader totals={:?}", agg.totals);
    println!("hand-run: count={} tokens_in={}", expected.0, expected.1);
    assert_eq!(
        agg.totals.requests, expected.0 as u64,
        "the reader must agree with the same query run by hand"
    );
    assert_eq!(agg.totals.tokens_in, expected.1 as u64);
    assert!(
        !agg.rows.is_empty(),
        "the grouped read must return the model group too"
    );
    println!("rows={:?}", agg.rows);
}
