use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use codex_code_router::config::{
    AppConfig, AuthConfig, CopilotHeaderConfig, RateLimitConfig, RawLogConfig,
    DEFAULT_GITHUB_ACCESS_TOKEN_URL, DEFAULT_GITHUB_DEVICE_CODE_URL,
    DEFAULT_GITHUB_OAUTH_CLIENT_ID, DEFAULT_GITHUB_OAUTH_SCOPE,
};
use codex_code_router::proxy::{app, AppState};
use futures_util::stream;
use serde_json::Value;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
struct RecordedRequest {
    headers: HeaderMap,
    body: Vec<u8>,
}

struct TestServer {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn spawn_router(router: Router) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    TestServer { addr, handle }
}

fn test_config(models_url: String, responses_url: String) -> AppConfig {
    AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        upstream_responses_url: responses_url,
        upstream_models_url: models_url,
        request_timeout: Duration::from_secs(30),
        headers: CopilotHeaderConfig {
            copilot_chat_version: "test-chat".to_owned(),
            copilot_editor_version: "vscode/test".to_owned(),
            github_api_version: "2025-10-01".to_owned(),
        },
        auth: AuthConfig {
            bearer_token: Some("service-owned-token".to_owned()),
            token_file: PathBuf::from("/definitely/not/present"),
            token_expiry_buffer: Duration::from_secs(300),
            refresh_enabled: true,
            copilot_token_url: "http://127.0.0.1/copilot-token".to_owned(),
            github_device_code_url: DEFAULT_GITHUB_DEVICE_CODE_URL.to_owned(),
            github_access_token_url: DEFAULT_GITHUB_ACCESS_TOKEN_URL.to_owned(),
            github_oauth_client_id: DEFAULT_GITHUB_OAUTH_CLIENT_ID.to_owned(),
            github_oauth_scope: DEFAULT_GITHUB_OAUTH_SCOPE.to_owned(),
        },
        rate_limit: RateLimitConfig {
            max_total_wait: Some(Duration::from_millis(100)),
            max_sleep: Duration::from_millis(2),
            initial_backoff: Duration::from_millis(1),
            backoff_multiplier: 2.0,
        },
        raw_log: RawLogConfig {
            enabled: false,
            file: PathBuf::from("/tmp/codex-code-router-test-raw.jsonl"),
            max_bytes: 4096,
        },
    }
}

async fn spawn_app(config: AppConfig) -> TestServer {
    spawn_router(app(AppState::new(config).unwrap())).await
}

fn enable_raw_log(config: &mut AppConfig, file: &NamedTempFile) {
    config.raw_log.enabled = true;
    config.raw_log.file = file.path().to_path_buf();
    config.raw_log.max_bytes = 16 * 1024;
}

fn raw_log_text(file: &NamedTempFile) -> String {
    fs::read_to_string(file.path()).unwrap_or_default()
}

fn raw_event_fields(text: &str, kind: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("kind").and_then(Value::as_str) == Some(kind))
        .filter_map(|event| event.get("fields").cloned())
        .collect()
}

fn assert_no_log_secret(text: &str, secret: &str) {
    assert!(
        !text.contains(secret),
        "diagnostic log leaked secret `{secret}`: {text}"
    );
}

fn assert_has_terminal_stream_event(text: &str) {
    assert!(
        text.contains("upstream_stream_completed") || text.contains("upstream_stream_dropped"),
        "expected a terminal stream diagnostic event with counts: {text}"
    );
    assert!(text.contains("chunk_count"));
    assert!(text.contains("byte_count"));
}

#[tokio::test]
async fn health_returns_service_status() {
    let config = test_config(
        "http://127.0.0.1/models".to_owned(),
        "http://127.0.0.1/responses".to_owned(),
    );
    let server = spawn_app(config).await;

    let response = reqwest::get(server.url("/health")).await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("codex-code-router"));
}

#[tokio::test]
async fn models_proxy_forwards_to_mocked_upstream_with_copilot_headers() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let mock = spawn_router({
        let recorded = recorded.clone();
        Router::new().route(
            "/models",
            get(move |headers: HeaderMap| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(RecordedRequest {
                        headers,
                        body: Vec::new(),
                    });
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        r#"{"data":[{"id":"gpt-test"}]}"#,
                    )
                }
            }),
        )
    })
    .await;

    let config = test_config(mock.url("/models"), mock.url("/responses"));
    let server = spawn_app(config).await;

    let response = reqwest::get(server.url("/v1/models")).await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"data":[{"id":"gpt-test"}]}"#);

    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let headers = &requests[0].headers;
    assert_eq!(
        headers.get("authorization").unwrap(),
        "Bearer service-owned-token"
    );
    assert_eq!(
        headers.get("copilot-integration-id").unwrap(),
        "vscode-chat"
    );
    assert_eq!(
        headers.get("editor-plugin-version").unwrap(),
        "copilot-chat/test-chat"
    );
    assert!(headers.get("accept").is_some());
}

#[tokio::test]
async fn models_raw_diagnostics_log_lifecycle_without_tokens() {
    let raw_log = NamedTempFile::new().unwrap();
    let mock = spawn_router(Router::new().route(
        "/models",
        get(|| async {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                r#"{"data":[{"id":"gpt-test"}]}"#,
            )
        }),
    ))
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;

    let response = reqwest::get(server.url("/v1/models")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.bytes().await.unwrap();

    let text = raw_log_text(&raw_log);
    assert!(text.contains("inbound_request"));
    assert!(text.contains("upstream_response_ready"));
    assert_has_terminal_stream_event(&text);
    assert!(text.contains("local_id"));
    assert!(text.contains("Models"));
    assert_no_log_secret(&text, "service-owned-token");

    let inbound = raw_event_fields(&text, "inbound_request");
    assert_eq!(inbound[0]["target"], "Models");
    assert_eq!(inbound[0]["body_len"], 0);
}

#[tokio::test]
async fn responses_proxy_streams_sse_bytes_unchanged() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let upstream_sse = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";

    let mock = spawn_router({
        let recorded = recorded.clone();
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, body: Bytes| {
                let recorded = recorded.clone();
                let upstream_sse = upstream_sse.clone();
                async move {
                    recorded.lock().unwrap().push(RecordedRequest {
                        headers,
                        body: body.to_vec(),
                    });
                    let chunks = stream::iter([
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[..48].to_owned(),
                        )),
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[48..].to_owned(),
                        )),
                    ]);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }
            }),
        )
    })
    .await;

    let config = test_config(mock.url("/models"), mock.url("/responses"));
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","stream":true,"input":"hello"}"#;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .header("content-type", "application/json")
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.bytes().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/event-stream"));
    assert_eq!(body.as_ref(), upstream_sse.as_bytes());

    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, request_body);
    assert_eq!(requests[0].headers.get("accept").unwrap(), "*/*");
}

#[tokio::test]
async fn responses_raw_diagnostics_log_lifecycle_and_stream_counts_without_body_or_token() {
    let raw_log = NamedTempFile::new().unwrap();
    let upstream_sse = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    let mock = spawn_router({
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move || {
                let upstream_sse = upstream_sse.clone();
                async move {
                    let chunks = stream::iter([
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[..48].to_owned(),
                        )),
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[48..].to_owned(),
                        )),
                    ]);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }
            }),
        )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"body-secret-that-must-not-log"}"#;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .header("content-type", "application/json")
        .header("x-codex-window-id", "window-1")
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.unwrap();
    assert_eq!(body.as_ref(), upstream_sse.as_bytes());

    let text = raw_log_text(&raw_log);
    assert!(text.contains("inbound_request"));
    assert!(text.contains("upstream_response_ready"));
    assert!(text.contains("upstream_stream_completed"));
    assert!(text.contains("local_id"));
    assert!(text.contains("Responses"));
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "body-secret-that-must-not-log");

    let inbound = raw_event_fields(&text, "inbound_request");
    assert_eq!(inbound[0]["target"], "Responses");
    assert_eq!(inbound[0]["body_len"], request_body.len());
    assert_eq!(
        inbound[0]["forwarded_codex_headers"][0],
        "x-codex-window-id"
    );

    let stream_events = raw_event_fields(&text, "upstream_stream_completed");
    assert_eq!(stream_events[0]["byte_count"], upstream_sse.len());
    assert!(stream_events[0]["chunk_count"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn codex_responses_body_is_not_normalized_before_forwarding() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let mock = spawn_router({
        let recorded = recorded.clone();
        Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, body: Bytes| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(RecordedRequest {
                        headers,
                        body: body.to_vec(),
                    });
                    (StatusCode::OK, "ok")
                }
            }),
        )
    })
    .await;
    let config = test_config(mock.url("/models"), mock.url("/responses"));
    let server = spawn_app(config).await;
    let codex_body = br#"{
  "model": "gpt-test",
  "stream": true,
  "store": true,
  "previous_response_id": "resp_should_remain_if_codex_sent_it",
  "include": ["reasoning.encrypted_content"],
  "reasoning": {"effort": "medium", "summary": "auto"},
  "tools": [{"type": "namespace", "name": "mcp__memory__recall"}],
  "input": [{"type": "reasoning", "encrypted_content": "opaque-provider-state"}]
}"#;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .header("content-type", "application/json")
        .body(codex_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body, codex_body,
        "The adapter is a provider shim; it must not rewrite Codex Responses fields unless a proven compatibility fix is added."
    );
}

#[tokio::test]
async fn in_band_rate_limit_text_is_streamed_without_retrying() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream_sse =
        "event: response.failed\ndata: {\"error\":{\"message\":\"rate limit exceeded\"}}\n\n";
    let mock = spawn_router({
        let attempts = attempts.clone();
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                let upstream_sse = upstream_sse.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(upstream_sse))
                        .unwrap()
                }
            }),
        )
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, upstream_sse);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "Only HTTP 429 status responses should trigger provider rate-limit retry."
    );
}

#[tokio::test]
async fn upstream_hop_by_hop_headers_are_not_exposed_to_codex() {
    let mock = spawn_router(Router::new().route(
        "/models",
        get(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header("connection", "close")
                .header("keep-alive", "timeout=5")
                .header("x-provider-trace", "visible")
                .body(Body::from(r#"{"data":[]}"#))
                .unwrap()
        }),
    ))
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = reqwest::get(server.url("/v1/models")).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-provider-trace").unwrap(),
        "visible"
    );
    assert!(
        response.headers().get("connection").is_none(),
        "Connection-specific upstream headers must not leak across the local proxy boundary."
    );
    assert!(
        response.headers().get("keep-alive").is_none(),
        "Hop-by-hop keep-alive metadata belongs to one network leg only."
    );
}

#[tokio::test]
async fn responses_retry_uses_retry_after_and_preserves_body() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(
        recorded.clone(),
        attempts.clone(),
        RetryMode::RetryAfterThenOk,
    )
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_sleep = Duration::from_millis(1);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"retry me"}"#;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let requests = recorded.lock().unwrap();
    assert_eq!(requests[0].body, request_body);
    assert_eq!(requests[1].body, request_body);
    assert_ne!(
        requests[0].headers.get("x-request-id"),
        requests[1].headers.get("x-request-id")
    );
}

#[tokio::test]
async fn retry_raw_diagnostics_include_wait_source_budget_and_correlation_without_body() {
    let raw_log = NamedTempFile::new().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = retry_mock(recorded, attempts.clone(), RetryMode::RetryAfterThenOk).await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_sleep = Duration::from_millis(1);
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"retry-body-secret"}"#;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let text = raw_log_text(&raw_log);
    assert!(text.contains("upstream_rate_limited"));
    assert!(text.contains("RetryAfter"));
    assert!(text.contains("local_id"));
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "retry-body-secret");
    let retry = raw_event_fields(&text, "upstream_rate_limited");
    assert_eq!(retry[0]["status"], StatusCode::TOO_MANY_REQUESTS.as_u16());
    assert_eq!(retry[0]["retry_budget_exceeded"], false);
    assert!(retry[0].get("budget_ms").is_some());
    assert!(retry[0].get("upstream_request_id").is_some());
}

#[tokio::test]
async fn responses_retry_uses_fallback_backoff_without_headers() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(recorded, attempts.clone(), RetryMode::BackoffThenOk).await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn responses_retry_uses_epoch_reset_header_and_preserves_body() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(
        recorded.clone(),
        attempts.clone(),
        RetryMode::ResetHeaderThenOk,
    )
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_sleep = Duration::from_millis(1);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"retry at reset"}"#;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests[0].body, request_body);
    assert_eq!(requests[1].body, request_body);
}

#[tokio::test]
async fn responses_retry_returns_429_after_positive_wait_budget_is_exceeded() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(recorded, attempts.clone(), RetryMode::AlwaysRateLimited).await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Some(Duration::ZERO);
    let server = spawn_app(config).await;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body, "still limited");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn budget_exhaustion_raw_diagnostics_record_decision_without_secrets() {
    let raw_log = NamedTempFile::new().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = retry_mock(recorded, attempts.clone(), RetryMode::AlwaysRateLimited).await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Some(Duration::ZERO);
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body("budget-body-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.text().await.unwrap(), "still limited");

    let text = raw_log_text(&raw_log);
    assert!(text.contains("upstream_rate_limited"));
    assert_has_terminal_stream_event(&text);
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "budget-body-secret");
    let retry = raw_event_fields(&text, "upstream_rate_limited");
    assert_eq!(retry[0]["retry_budget_exceeded"], true);
}

#[tokio::test]
async fn send_failure_raw_diagnostics_are_correlated_and_redact_url_queries() {
    let raw_log = NamedTempFile::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut config = test_config(
        format!("http://{addr}/models?token=query-secret"),
        format!("http://{addr}/responses?token=query-secret"),
    );
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body("send-failure-body-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let response_body = response.text().await.unwrap();

    let text = raw_log_text(&raw_log);
    assert!(text.contains("upstream_request_failed"));
    assert!(text.contains("local_id"));
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "query-secret");
    assert_no_log_secret(&text, "send-failure-body-secret");
    assert_no_log_secret(&response_body, "query-secret");
    assert_no_log_secret(&response_body, "send-failure-body-secret");
}

#[tokio::test]
async fn non_429_errors_are_not_rate_limit_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::SERVICE_UNAVAILABLE, "not a rate limit")
                }
            }),
        )
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = reqwest::Client::new()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_auth_returns_401_without_calling_upstream() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/models",
            get(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "should not be called")
                }
            }),
        )
    })
    .await;

    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.auth.bearer_token = None;
    let server = spawn_app(config).await;

    let response = reqwest::get(server.url("/v1/models")).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn wrong_method_for_responses_is_rejected_without_calling_upstream() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "should not be called")
                }
            }),
        )
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = reqwest::get(server.url("/v1/responses")).await.unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "Unsupported local methods should be rejected before any upstream provider call."
    );
}

#[tokio::test]
async fn unsupported_routes_return_not_found() {
    let config = test_config(
        "http://127.0.0.1/models".to_owned(),
        "http://127.0.0.1/responses".to_owned(),
    );
    let server = spawn_app(config).await;

    let response = reqwest::get(server.url("/v1/chat/completions"))
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Supported endpoints"));
}

#[derive(Clone, Copy)]
enum RetryMode {
    RetryAfterThenOk,
    ResetHeaderThenOk,
    BackoffThenOk,
    AlwaysRateLimited,
}

async fn retry_mock(
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    attempts: Arc<AtomicUsize>,
    mode: RetryMode,
) -> TestServer {
    spawn_router(Router::new().route(
        "/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let recorded = recorded.clone();
            let attempts = attempts.clone();
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                recorded.lock().unwrap().push(RecordedRequest {
                    headers,
                    body: body.to_vec(),
                });

                match (mode, attempt) {
                    (RetryMode::RetryAfterThenOk, 0) => Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header("retry-after", "1")
                        .body(Body::from("limited once"))
                        .unwrap(),
                    (RetryMode::ResetHeaderThenOk, 0) => {
                        let reset_epoch_seconds = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 60;
                        Response::builder()
                            .status(StatusCode::TOO_MANY_REQUESTS)
                            .header("x-ratelimit-reset", reset_epoch_seconds.to_string())
                            .body(Body::from("limited until reset"))
                            .unwrap()
                    }
                    (RetryMode::BackoffThenOk, 0) => {
                        (StatusCode::TOO_MANY_REQUESTS, "limited once").into_response()
                    }
                    (RetryMode::AlwaysRateLimited, _) => {
                        (StatusCode::TOO_MANY_REQUESTS, "still limited").into_response()
                    }
                    _ => (StatusCode::OK, "ok after retry").into_response(),
                }
            }
        }),
    ))
    .await
}
