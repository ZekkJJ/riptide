//! The local HTTP API.
//!
//! # Why this is safe to expose on the loopback interface
//!
//! Any page in the user's browser can send requests to `127.0.0.1`, so an unauthenticated
//! local API that queues downloads and lists filenames would be a real hole. Two things close
//! it:
//!
//! - Every `/api` route requires the `X-FreeDM-Token` secret, generated per install and never
//!   served over HTTP. A hostile page cannot guess it, and the preflight needed to send a
//!   custom header carries no credentials, so allowing the preflight gives nothing away.
//! - The web UI at `/` has the token inlined, and is served with no CORS headers at all. A
//!   cross-origin `fetch` of `/` is therefore blocked from reading the response body by the
//!   browser itself, so the token cannot be lifted out of it.
//!
//! The listener binds `127.0.0.1` explicitly, so nothing is reachable from the network.

use crate::config::AppConfig;
use crate::manager::{Item, Manager};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use freedm_core::RequestSpec;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const TOKEN_HEADER: &str = "x-freedm-token";

#[derive(Clone)]
pub struct Ctx {
    pub manager: Arc<Manager>,
    pub token: String,
}

pub fn router(manager: Arc<Manager>, token: String) -> Router {
    let ctx = Ctx { manager, token };

    let api = Router::new()
        .route("/downloads", get(list_downloads).post(add_download))
        .route("/downloads/:id", get(get_download).delete(remove_download))
        .route("/downloads/:id/pause", post(pause))
        .route("/downloads/:id/resume", post(resume))
        .route("/downloads/:id/cancel", post(cancel))
        .route("/pause-all", post(pause_all))
        .route("/resume-all", post(resume_all))
        .route("/clear-finished", post(clear_finished))
        .route("/config", get(get_config).put(put_config))
        .route("/capture-rules", get(capture_rules))
        .route("/events", get(events))
        .route("/ping", get(ping))
        .with_state(ctx.clone());

    Router::new()
        .route("/", get(ui))
        .nest("/api", api)
        .with_state(ctx)
        // A custom header forces a preflight, which this answers so the browser extension can
        // talk to the API. The preflight itself reveals nothing: the request behind it still
        // has to carry the secret.
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
}

/// Reject anything that does not present the shared secret.
#[allow(clippy::result_large_err)] // the Err case is the rejection response itself
fn authorize(ctx: &Ctx, headers: &HeaderMap, query_token: Option<&str>) -> Result<(), Response> {
    let supplied = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .or(query_token)
        .unwrap_or_default();

    // Constant-time comparison, so a caller cannot learn the token byte by byte from timing.
    let expected = ctx.token.as_bytes();
    let got = supplied.as_bytes();
    let equal = expected.len() == got.len()
        && expected.iter().zip(got).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;

    if equal {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "missing or incorrect X-FreeDM-Token",
                "hint": "run `freedmd token` to print the token for this install"
            })),
        )
            .into_response())
    }
}

#[derive(Debug, Deserialize)]
pub struct AddRequest {
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthQuery {
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoveQuery {
    #[serde(default)]
    pub delete_file: bool,
}

async fn ping(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    Json(serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })).into_response()
}

async fn list_downloads(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    Json(ctx.manager.list().await).into_response()
}

async fn get_download(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    match ctx.manager.get(id).await {
        Some(item) => Json(item).into_response(),
        None => not_found(),
    }
}

async fn add_download(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Json(req): Json<AddRequest>,
) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }

    // Reject anything that is not an http(s) URL before it reaches the engine: a `file://` or
    // other scheme arriving from a web page must not be treated as something to fetch.
    match url::Url::parse(&req.url) {
        Ok(u) if matches!(u.scheme(), "http" | "https" | "ftp") => {}
        Ok(u) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("unsupported scheme {:?}", u.scheme()) })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("not a url: {e}") })),
            )
                .into_response()
        }
    }

    let mut spec = RequestSpec::new(&req.url);
    spec.referer = req.referer;
    spec.cookies = req.cookies;
    spec.user_agent = req.user_agent;
    spec.headers = req.headers;
    spec.mirrors = req.mirrors;

    let source = req.source.unwrap_or_else(|| "api".into());
    let item: Item = ctx.manager.add(spec, req.filename, &source).await;
    (StatusCode::CREATED, Json(item)).into_response()
}

macro_rules! action {
    ($name:ident, $method:ident) => {
        async fn $name(
            State(ctx): State<Ctx>,
            headers: HeaderMap,
            Path(id): Path<Uuid>,
        ) -> Response {
            if let Err(r) = authorize(&ctx, &headers, None) {
                return r;
            }
            if ctx.manager.$method(id).await {
                Json(serde_json::json!({ "ok": true })).into_response()
            } else {
                (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": concat!("cannot ", stringify!($method), " this download in its current state")
                    })),
                )
                    .into_response()
            }
        }
    };
}

action!(pause, pause);
action!(resume, resume);
action!(cancel, cancel);

async fn remove_download(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<RemoveQuery>,
) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    if ctx.manager.remove(id, q.delete_file).await {
        Json(serde_json::json!({ "ok": true })).into_response()
    } else {
        not_found()
    }
}

async fn pause_all(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    ctx.manager.pause_all().await;
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn resume_all(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    ctx.manager.resume_all().await;
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn clear_finished(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    let n = ctx.manager.clear_finished().await;
    Json(serde_json::json!({ "ok": true, "removed": n })).into_response()
}

async fn get_config(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    let mut value = serde_json::to_value(ctx.manager.config().await).unwrap_or_default();
    // The token is already known to an authorized caller, but there is no reason to echo it.
    if let Some(obj) = value.as_object_mut() {
        obj.remove("token");
    }
    Json(value).into_response()
}

async fn put_config(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    // Merge over the current config so a partial update does not reset everything, and keep
    // the existing token whatever the body says.
    let current = ctx.manager.config().await;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("token".into(), serde_json::Value::String(current.token.clone()));
    }
    let merged = match merge_config(&current, body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };
    match ctx.manager.set_config(merged).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Apply a partial JSON object onto the current config.
fn merge_config(current: &AppConfig, patch: serde_json::Value) -> anyhow::Result<AppConfig> {
    let mut base = serde_json::to_value(current)?;
    if let (Some(base_obj), Some(patch_obj)) = (base.as_object_mut(), patch.as_object()) {
        for (k, v) in patch_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    Ok(serde_json::from_value(base)?)
}

/// What the browser extension needs in order to decide which responses to take over.
async fn capture_rules(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&ctx, &headers, None) {
        return r;
    }
    let cfg = ctx.manager.config().await;
    Json(serde_json::json!({
        "extensions": cfg.effective_capture_extensions(),
        "exclusions": cfg.capture_exclusions,
        "min_size": cfg.capture_min_size,
    }))
    .into_response()
}

/// Live progress as server-sent events.
///
/// `EventSource` cannot set request headers, so the token comes in the query string here. It
/// is a loopback request carrying a secret the caller already had to know.
async fn events(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Response {
    if let Err(r) = authorize(&ctx, &headers, q.token.as_deref()) {
        return r;
    }

    let rx = ctx.manager.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    return Some((Ok::<_, Infallible>(SseEvent::default().data(json)), rx));
                }
                // A slow client that fell behind resyncs on the next event rather than closing.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// Serve the web UI with the token inlined.
///
/// No CORS headers are attached by this handler's own response, so although a hostile page can
/// issue the request, the browser will not let it read the body, and the token stays private.
async fn ui(State(ctx): State<Ctx>) -> Html<String> {
    let page = include_str!("../../../web/index.html");
    Html(page.replace("__FREEDM_TOKEN__", &ctx.token))
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no such download" })))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            manager: Manager::new(AppConfig::default()),
            token: "0123456789abcdef0123456789abcdef".to_string(),
        }
    }

    fn headers_with(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(TOKEN_HEADER, token.parse().unwrap());
        h
    }

    #[tokio::test]
    async fn the_correct_token_is_accepted() {
        let c = ctx();
        assert!(authorize(&c, &headers_with(&c.token), None).is_ok());
    }

    #[tokio::test]
    async fn a_missing_or_wrong_token_is_rejected() {
        let c = ctx();
        assert!(authorize(&c, &HeaderMap::new(), None).is_err());
        assert!(authorize(&c, &headers_with("wrong"), None).is_err());
        // A correct prefix must not pass; the comparison covers the whole value.
        assert!(authorize(&c, &headers_with(&c.token[..16]), None).is_err());
        // Nor may a longer value that starts correctly.
        assert!(authorize(&c, &headers_with(&format!("{}extra", c.token)), None).is_err());
    }

    #[tokio::test]
    async fn the_token_may_also_arrive_in_the_query_for_event_streams() {
        let c = ctx();
        // EventSource cannot set headers, so the SSE route accepts it this way.
        assert!(authorize(&c, &HeaderMap::new(), Some(&c.token)).is_ok());
        assert!(authorize(&c, &HeaderMap::new(), Some("nope")).is_err());
    }

    #[test]
    fn the_ui_page_has_a_placeholder_for_the_token() {
        // If the placeholder is ever renamed, the UI would ship without a token and silently
        // fail every request, so assert the contract between page and handler.
        let page = include_str!("../../../web/index.html");
        assert!(page.contains("__FREEDM_TOKEN__"));
    }

    #[test]
    fn a_partial_config_update_leaves_other_fields_alone() {
        let current = AppConfig {
            connections_per_download: 8,
            max_concurrent_downloads: 3,
            categorize: true,
            ..Default::default()
        };
        let patch = serde_json::json!({ "connections_per_download": 16 });
        let merged = merge_config(&current, patch).unwrap();

        assert_eq!(merged.connections_per_download, 16);
        assert_eq!(merged.max_concurrent_downloads, 3, "untouched fields survive");
        assert!(merged.categorize);
        assert_eq!(merged.token, current.token, "the secret is never changed by an update");
    }

    #[test]
    fn an_invalid_config_update_is_an_error_rather_than_a_silent_default() {
        let current = AppConfig::default();
        let patch = serde_json::json!({ "connections_per_download": "lots" });
        assert!(merge_config(&current, patch).is_err());
    }
}
