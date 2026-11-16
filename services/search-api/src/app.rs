//! HTTP routes. All responses are JSON except the static page.
//!
//! Bad input always gets a 400 with `{"error": "..."}`; a request that arrives
//! before the index has finished loading gets a 503. Search runs on the
//! blocking pool so slow queries never stall the async workers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::engine::{Engine, SearchResponse};
use crate::latency::Latencies;

pub const MAX_QUERY_CHARS: usize = 256;
pub const MAX_K: usize = 100;
pub const MAX_OFFSET: usize = 10_000;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<OnceLock<Arc<Engine>>>,
    pub latencies: Arc<Mutex<Latencies>>,
    pub started: Instant,
    /// At most one search per core runs at a time; the rest wait here, in
    /// arrival order. Without it, 50 concurrent searches time-slice 12 cores
    /// and every one of them gets slower.
    pub search_slots: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    pub fn new() -> Self {
        Self::with_search_slots(std::thread::available_parallelism().map_or(4, std::num::NonZero::get))
    }

    pub fn with_search_slots(slots: usize) -> Self {
        Self {
            engine: Arc::new(OnceLock::new()),
            latencies: Arc::new(Mutex::new(Latencies::new(100_000))),
            started: Instant::now(),
            search_slots: Arc::new(tokio::sync::Semaphore::new(slots.max(1))),
        }
    }

    fn engine(&self) -> Result<Arc<Engine>, ApiError> {
        self.engine
            .get()
            .cloned()
            .ok_or_else(|| ApiError(StatusCode::SERVICE_UNAVAILABLE, "index is still loading".into()))
    }
}

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/search", get(search))
        .route("/doc/{id}", get(doc))
        .route("/health", get(health))
        .route("/stats", get(stats))
        .with_state(state)
}

async fn index_html() -> Html<&'static str> {
    Html(include_str!("../../../web/index.html"))
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../../../web/app.js"))
}

async fn style_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../../../web/style.css"))
}

fn parse_usize(params: &HashMap<String, String>, key: &str, default: usize) -> Result<usize, ApiError> {
    match params.get(key).map(|s| s.trim()) {
        None | Some("") => Ok(default),
        Some(s) => s.parse().map_err(|_| bad(format!("{key} must be a non-negative integer"))),
    }
}

async fn search(
    State(st): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<SearchResponse>, ApiError> {
    let q = params.get("q").map(|s| s.trim()).unwrap_or_default().to_owned();
    if q.is_empty() {
        return Err(bad("query is empty"));
    }
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(bad(format!("query is longer than {MAX_QUERY_CHARS} characters")));
    }
    let k = parse_usize(&params, "k", 10)?;
    if k == 0 || k > MAX_K {
        return Err(bad(format!("k must be between 1 and {MAX_K}")));
    }
    let offset = parse_usize(&params, "offset", 0)?;
    if offset > MAX_OFFSET {
        return Err(bad(format!("offset must be at most {MAX_OFFSET}")));
    }
    let engine = st.engine()?;

    let _slot = st
        .search_slots
        .acquire()
        .await
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "shutting down".into()))?;
    let resp = tokio::task::spawn_blocking(move || engine.search(&q, k, offset))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("search task failed: {e}")))?
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("search failed: {e}")))?;

    if let Ok(mut l) = st.latencies.lock() {
        l.record(resp.took_ms);
    }
    Ok(Json(resp))
}

async fn doc(State(st): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id: u32 = id.parse().map_err(|_| bad("doc id must be a non-negative integer"))?;
    let engine = st.engine()?;
    if id >= engine.store.num_docs() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no doc {id}")));
    }
    let d = tokio::task::spawn_blocking(move || engine.store.get(id))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "doc_id": d.id, "title": d.title, "url": d.url, "text": d.text })).into_response())
}

async fn health(State(st): State<AppState>) -> Response {
    match st.engine.get() {
        Some(e) => Json(json!({
            "status": "ok",
            "index_version": e.index_version(),
            "num_docs": e.body.meta().num_docs,
        }))
        .into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "loading" }))).into_response(),
    }
}

async fn stats(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let e = st.engine()?;
    let (p50, p95, p99, count) = match st.latencies.lock() {
        Ok(l) => (l.percentile(50.0), l.percentile(95.0), l.percentile(99.0), l.count),
        Err(_) => (None, None, None, 0),
    };
    let m = e.body.meta();
    Ok(Json(json!({
        "num_docs": m.num_docs,
        "num_terms": m.num_terms,
        "num_title_terms": e.title.as_ref().map(|t| t.meta().num_terms),
        "index_bytes": e.index_bytes,
        "avgdl": m.avgdl,
        "searches": count,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "uptime_s": st.started.elapsed().as_secs(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use oxdex_docstore::DocStoreWriter;
    use oxdex_index::{IndexBuilder, count_terms};
    use tower::ServiceExt;

    const DOCS: &[(&str, &str)] = &[
        ("Field-programmable gate array", "An FPGA is a chip that can be rewired after manufacturing. Timing closure matters."),
        ("Xilinx", "Xilinx made FPGA devices and timing tools."),
        ("Rust (programming language)", "Rust is a systems programming language focused on safety & speed."),
        ("Potato", "The potato is a starchy tuber."),
    ];

    fn fixture() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let (store, index) = (dir.path().join("store"), dir.path().join("index"));
        let mut w = DocStoreWriter::create(&store).unwrap();
        let mut body = IndexBuilder::create(&index, 1 << 20).unwrap();
        let mut title = IndexBuilder::create(index.join("title"), 1 << 20).unwrap();
        for (t, text) in DOCS {
            w.add(t, &format!("https://example.org/{t}"), text, &[]).unwrap();
            let (terms, len) = count_terms(t, text);
            body.add_doc(&terms, len).unwrap();
            let (terms, len) = count_terms(t, "");
            title.add_doc(&terms, len).unwrap();
        }
        w.finish().unwrap();
        title.finish().unwrap();
        body.finish().unwrap();
        let state = AppState::new();
        let _ = state.engine.set(Arc::new(Engine::open(&index, &store).unwrap()));
        (dir, state)
    }

    async fn get(state: AppState, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = router(state).oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn search_returns_ranked_results_with_snippets() {
        let (_d, st) = fixture();
        let (s, v) = get(st, "/search?q=fpga+rewired").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total_hits"], 2); // fpga OR rewired
        // Two hits is under min_and_hits (10), so the spec's OR fallback kicks in.
        assert_eq!(v["mode"], "and_fell_back_to_or");
        assert!(v["took_ms"].as_f64().unwrap() >= 0.0);
        let first = &v["results"][0];
        assert_eq!(first["title"], "Field-programmable gate array");
        assert!(first["snippet"].as_str().unwrap().contains("<mark>FPGA</mark>"));
        assert!(first["snippet"].as_str().unwrap().contains("<mark>rewired</mark>"));
    }

    #[tokio::test]
    async fn exclusion_and_paging() {
        let (_d, st) = fixture();
        let (_, v) = get(st.clone(), "/search?q=fpga+-xilinx").await;
        assert_eq!(v["total_hits"], 1);
        let (_, v) = get(st, "/search?q=fpga&k=1&offset=1").await;
        assert_eq!(v["results"].as_array().unwrap().len(), 1);
        assert_eq!(v["offset"], 1);
    }

    #[tokio::test]
    async fn snippets_escape_html() {
        let (_d, st) = fixture();
        let (_, v) = get(st, "/search?q=safety").await;
        let snip = v["results"][0]["snippet"].as_str().unwrap();
        assert!(snip.contains("&amp;"), "{snip}");
    }

    #[tokio::test]
    async fn bad_input_is_a_400_with_json_error() {
        let (_d, st) = fixture();
        let long = "a".repeat(257);
        for uri in [
            "/search".to_owned(),
            "/search?q=".to_owned(),
            "/search?q=%20%20".to_owned(),
            format!("/search?q={long}"),
            "/search?q=rust&k=0".to_owned(),
            "/search?q=rust&k=101".to_owned(),
            "/search?q=rust&k=abc".to_owned(),
            "/search?q=rust&offset=-1".to_owned(),
            "/search?q=rust&offset=99999999".to_owned(),
            "/doc/abc".to_owned(),
        ] {
            let (s, v) = get(st.clone(), &uri).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}");
            assert!(v["error"].is_string(), "{uri}: {v}");
        }
        let (s, _) = get(st.clone(), &format!("/search?q={}", "a".repeat(256))).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = get(st, "/doc/4").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn odd_queries_never_500() {
        let (_d, st) = fixture();
        for q in ["OR", "-", "--", "-rust", "OR+OR+-", "%E2%9C%93", "the", "%3Cscript%3E", "zzzz"] {
            let (s, v) = get(st.clone(), &format!("/search?q={q}")).await;
            assert_eq!(s, StatusCode::OK, "{q}: {v}");
        }
    }

    #[tokio::test]
    async fn loading_is_a_503() {
        let st = AppState::new();
        for uri in ["/search?q=rust", "/health", "/stats", "/doc/0"] {
            let (s, _) = get(st.clone(), uri).await;
            assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
        // Bad input still wins over loading.
        let (s, _) = get(st, "/search?q=").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn health_stats_doc_and_page() {
        let (_d, st) = fixture();
        let (s, v) = get(st.clone(), "/health").await;
        assert_eq!((s, &v["status"]), (StatusCode::OK, &json!("ok")));
        let _ = get(st.clone(), "/search?q=rust").await;
        let (_, v) = get(st.clone(), "/stats").await;
        assert_eq!(v["num_docs"], 4);
        assert_eq!(v["searches"], 1);
        assert!(v["latency_ms"]["p95"].is_number());
        let (_, v) = get(st.clone(), "/doc/3").await;
        assert_eq!(v["title"], "Potato");
        let resp = router(st).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
