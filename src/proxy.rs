use crate::config::AppConfig;
use crate::diagnostics::write_raw_event;
use crate::headers::{build_upstream_headers, forwarded_codex_header_names};
use crate::redaction::{hash_and_truncate, redact_headers, redact_url, truncate_for_log};
use crate::retry::{retry_budget_exceeded, select_retry_wait};
use crate::token::{resolve_upstream_authorization, AuthError};
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use futures_util::Stream;
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const HOP_BY_HOP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone)]
pub struct AppState {
    config: Arc<AppConfig>,
    client: reqwest::Client,
}

impl AppState {
    pub fn new(config: AppConfig) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()?;

        Ok(Self {
            config: Arc::new(config),
            client,
        })
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .fallback(not_found)
        .with_state(state)
}

pub async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let bind = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&bind).await?;
    let addr = listener.local_addr()?;
    let config_summary = config.safe_summary();
    let upstream = redact_url(&config.upstream_responses_url);
    let state = AppState::new(config)?;

    info!(
        %addr,
        upstream_responses_url = %upstream,
        config = ?config_summary,
        "codex-code-router listening"
    );
    if state.config.raw_log.enabled {
        warn!(
            raw_log_file = %state.config.raw_log.file.display(),
            max_bytes = state.config.raw_log.max_bytes,
            "raw diagnostics are enabled; metadata is redacted and bounded but may still reveal request/tool context"
        );
    }

    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn health(State(state): State<AppState>) -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "service": "codex-code-router",
            "upstreamResponsesUrl": &state.config.upstream_responses_url,
            "upstreamModelsUrl": &state.config.upstream_models_url,
        }),
    )
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    forward(
        state,
        Method::GET,
        Target::Models,
        headers,
        None,
        "application/json",
        false,
    )
    .await
}

async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    forward(
        state,
        Method::POST,
        Target::Responses,
        headers,
        Some(body),
        "text/event-stream",
        true,
    )
    .await
}

async fn not_found() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        json!({
            "error": "not_found",
            "message": "Supported endpoints: GET /health, GET /v1/models, POST /v1/responses."
        }),
    )
}

async fn forward(
    state: AppState,
    method: Method,
    target: Target,
    inbound_headers: HeaderMap,
    body: Option<Bytes>,
    default_accept: &'static str,
    default_content_type: bool,
) -> Response {
    let local_id = Uuid::new_v4().to_string();
    let body_len = body.as_ref().map(|body| body.len()).unwrap_or_default();
    let content_type = header_value(&inbound_headers, CONTENT_TYPE);
    let accept = header_value(&inbound_headers, ACCEPT);
    let forwarded_codex_headers = forwarded_codex_header_names(&inbound_headers);

    info!(
        local_id,
        method = %method,
        target = target.as_str(),
        body_len,
        content_type = ?content_type,
        accept = ?accept,
        forwarded_codex_headers = ?forwarded_codex_headers,
        "inbound request"
    );
    write_raw_event(
        &state.config.raw_log,
        "inbound_request",
        json!({
            "local_id": &local_id,
            "method": method.as_str(),
            "target": target.as_str(),
            "body_len": body_len,
            "content_type": content_type,
            "accept": accept,
            "forwarded_codex_headers": forwarded_codex_headers,
        }),
    );

    let authorization = match resolve_upstream_authorization(
        &state.config.auth,
        &state.config.headers,
        &state.client,
        &inbound_headers,
    )
    .await
    {
        Ok(authorization) => {
            info!(
                local_id,
                auth_source = ?authorization.source(),
                "resolved upstream authorization"
            );
            write_raw_event(
                &state.config.raw_log,
                "auth_resolved",
                json!({
                    "local_id": &local_id,
                    "auth_source": format!("{:?}", authorization.source()),
                }),
            );
            authorization
        }
        Err(error) => {
            log_auth_error(&local_id, target, &state, &error);
            return auth_error_response(error);
        }
    };

    let url = target.url(&state.config).to_owned();
    let redacted_url = redact_url(&url);
    let mut attempt = 0_u32;
    let mut total_wait = Duration::ZERO;

    loop {
        let request_id = Uuid::new_v4().to_string();
        let request_id_summary = hash_and_truncate(&request_id);
        let attempt_number = attempt.saturating_add(1);
        let upstream_headers = match build_upstream_headers(
            &inbound_headers,
            authorization.header_value(),
            &state.config.headers,
            default_accept,
            &request_id,
            default_content_type,
        ) {
            Ok(headers) => headers,
            Err(error) => {
                warn!(
                    local_id,
                    target = target.as_str(),
                    attempt = attempt_number,
                    error = %error,
                    "failed to build upstream headers"
                );
                write_raw_event(
                    &state.config.raw_log,
                    "upstream_header_build_failed",
                    json!({
                        "local_id": &local_id,
                        "target": target.as_str(),
                        "attempt": attempt_number,
                        "error": error.to_string(),
                    }),
                );
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "error": "upstream_header_build_failed",
                        "message": error.to_string(),
                    }),
                );
            }
        };

        debug!(
            local_id,
            target = target.as_str(),
            attempt = attempt_number,
            upstream_url = %redacted_url,
            body_len,
            upstream_request_id = %request_id_summary,
            upstream_headers = ?redact_headers(&upstream_headers),
            "upstream attempt starting"
        );

        let mut request = state
            .client
            .request(method.clone(), &url)
            .headers(upstream_headers);

        if let Some(body) = body.clone() {
            request = request.body(body);
        }

        let attempt_started = Instant::now();
        let upstream = match request.send().await {
            Ok(response) => {
                let elapsed = attempt_started.elapsed();
                debug!(
                    local_id,
                    target = target.as_str(),
                    attempt = attempt_number,
                    status = %response.status(),
                    elapsed_ms = elapsed.as_millis(),
                    upstream_request_id = %request_id_summary,
                    "upstream attempt completed"
                );
                response
            }
            Err(error) => {
                let elapsed = attempt_started.elapsed();
                warn!(
                    local_id,
                    target = target.as_str(),
                    attempt = attempt_number,
                    upstream_url = %redacted_url,
                    elapsed_ms = elapsed.as_millis(),
                    error_kind = classify_reqwest_error(&error),
                    error = %safe_reqwest_error(&error),
                    "upstream request failed"
                );
                write_raw_event(
                    &state.config.raw_log,
                    "upstream_request_failed",
                    json!({
                        "local_id": &local_id,
                        "target": target.as_str(),
                        "attempt": attempt_number,
                        "upstream_url": redacted_url,
                        "elapsed_ms": elapsed.as_millis(),
                        "error_kind": classify_reqwest_error(&error),
                        "error": safe_reqwest_error(&error),
                    }),
                );
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "error": "upstream_request_failed",
                        "message": safe_reqwest_error(&error),
                    }),
                );
            }
        };

        if upstream.status() != StatusCode::TOO_MANY_REQUESTS {
            let status = upstream.status();
            let content_type = header_value(upstream.headers(), CONTENT_TYPE);
            let elapsed = attempt_started.elapsed();
            info!(
                local_id,
                target = target.as_str(),
                attempt_count = attempt_number,
                status = %status,
                content_type = ?content_type,
                elapsed_ms = elapsed.as_millis(),
                "upstream response ready; streaming to client"
            );
            write_raw_event(
                &state.config.raw_log,
                "upstream_response_ready",
                json!({
                    "local_id": &local_id,
                    "target": target.as_str(),
                    "attempt_count": attempt_number,
                    "status": status.as_u16(),
                    "content_type": content_type,
                    "elapsed_ms": elapsed.as_millis(),
                }),
            );
            return response_from_upstream(
                upstream,
                state.config.raw_log.clone(),
                local_id,
                target,
            );
        }

        let wait = select_retry_wait(
            upstream.headers(),
            attempt,
            &state.config.rate_limit,
            SystemTime::now(),
        );
        let budget_exceeded = retry_budget_exceeded(
            total_wait,
            wait.delay,
            state.config.rate_limit.max_total_wait,
        );
        let total_wait_after = total_wait.saturating_add(wait.delay);
        let budget_ms = state
            .config
            .rate_limit
            .max_total_wait
            .map(|value| value.as_millis());
        warn!(
            local_id,
            target = target.as_str(),
            attempt = attempt_number,
            status = %StatusCode::TOO_MANY_REQUESTS,
            wait_ms = wait.delay.as_millis(),
            raw_wait_ms = wait.raw_delay.as_millis(),
            wait_clamped = wait.clamped,
            wait_source = ?wait.source,
            total_wait_before_ms = total_wait.as_millis(),
            total_wait_after_ms = total_wait_after.as_millis(),
            budget_ms = ?budget_ms,
            retry_budget_exceeded = budget_exceeded,
            upstream_request_id = %request_id_summary,
            "upstream rate-limited request"
        );
        write_raw_event(
            &state.config.raw_log,
            "upstream_rate_limited",
            json!({
                "local_id": &local_id,
                "target": target.as_str(),
                "attempt": attempt_number,
                "status": StatusCode::TOO_MANY_REQUESTS.as_u16(),
                "wait_ms": wait.delay.as_millis(),
                "raw_wait_ms": wait.raw_delay.as_millis(),
                "wait_clamped": wait.clamped,
                "wait_source": format!("{:?}", wait.source),
                "total_wait_before_ms": total_wait.as_millis(),
                "total_wait_after_ms": total_wait_after.as_millis(),
                "budget_ms": budget_ms,
                "retry_budget_exceeded": budget_exceeded,
                "upstream_request_id": request_id_summary,
            }),
        );
        if retry_budget_exceeded(
            total_wait,
            wait.delay,
            state.config.rate_limit.max_total_wait,
        ) {
            warn!(
                local_id,
                target = target.as_str(),
                attempt = attempt_number,
                total_wait_ms = total_wait.as_millis(),
                next_wait_ms = wait.delay.as_millis(),
                budget_ms = ?budget_ms,
                "rate-limit retry budget exceeded; returning upstream 429"
            );
            return response_from_upstream(
                upstream,
                state.config.raw_log.clone(),
                local_id,
                target,
            );
        }

        attempt = attempt.saturating_add(1);
        tokio::time::sleep(wait.delay).await;
        total_wait = total_wait_after;
    }
}

fn log_auth_error(local_id: &str, target: Target, state: &AppState, error: &AuthError) {
    let status = auth_error_status(error);
    let level = if status == StatusCode::UNAUTHORIZED {
        "warn"
    } else {
        "error"
    };

    if status == StatusCode::UNAUTHORIZED {
        warn!(
            local_id,
            target = target.as_str(),
            status = %status,
            error_class = auth_error_class(error),
            error = %error,
            "upstream authorization unavailable"
        );
    } else {
        error!(
            local_id,
            target = target.as_str(),
            status = %status,
            error_class = auth_error_class(error),
            error = %error,
            "upstream authorization unavailable"
        );
    }

    write_raw_event(
        &state.config.raw_log,
        "auth_error",
        json!({
            "local_id": local_id,
            "target": target.as_str(),
            "status": status.as_u16(),
            "level": level,
            "error_class": auth_error_class(error),
            "error": error.to_string(),
        }),
    );
}

fn auth_error_response(error: AuthError) -> Response {
    let status = auth_error_status(&error);

    json_response(
        status,
        json!({
            "error": "copilot_auth_unavailable",
            "message": error.to_string(),
        }),
    )
}

fn auth_error_status(error: &AuthError) -> StatusCode {
    match error {
        AuthError::Missing
        | AuthError::Expired { .. }
        | AuthError::ExpiredMissingGithubToken { .. }
        | AuthError::InvalidIncomingAuthorization => StatusCode::UNAUTHORIZED,
        AuthError::ReadTokenFile { .. }
        | AuthError::ParseTokenFile { .. }
        | AuthError::MissingCopilotToken { .. }
        | AuthError::RefreshStatus { .. }
        | AuthError::RefreshRequest { .. }
        | AuthError::WriteTokenFile { .. }
        | AuthError::MissingRefreshFields => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn response_from_upstream(
    upstream: reqwest::Response,
    raw_log: crate::config::RawLogConfig,
    local_id: String,
    target: Target,
) -> Response {
    let status = upstream.status();
    let mut builder = Response::builder().status(status);

    for (name, value) in upstream.headers() {
        if !is_hop_by_hop(name) {
            builder = builder.header(name.clone(), value.clone());
        }
    }

    let stream = LoggedByteStream::new(
        upstream.bytes_stream(),
        raw_log,
        local_id,
        target,
        status.as_u16(),
    );
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|error| {
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "error": "upstream_response_build_failed",
                    "message": error.to_string(),
                }),
            )
        })
}

struct LoggedByteStream<S> {
    inner: Pin<Box<S>>,
    raw_log: crate::config::RawLogConfig,
    local_id: String,
    target: Target,
    status: u16,
    started: Instant,
    chunk_count: u64,
    byte_count: u64,
    completed: bool,
}

impl<S> LoggedByteStream<S> {
    fn new(
        inner: S,
        raw_log: crate::config::RawLogConfig,
        local_id: String,
        target: Target,
        status: u16,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            raw_log,
            local_id,
            target,
            status,
            started: Instant::now(),
            chunk_count: 0,
            byte_count: 0,
            completed: false,
        }
    }
}

impl<S> Stream for LoggedByteStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.chunk_count = self.chunk_count.saturating_add(1);
                self.byte_count = self.byte_count.saturating_add(chunk.len() as u64);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.completed = true;
                let elapsed = self.started.elapsed();
                warn!(
                    local_id = %self.local_id,
                    target = self.target.as_str(),
                    status = self.status,
                    chunk_count = self.chunk_count,
                    byte_count = self.byte_count,
                    elapsed_ms = elapsed.as_millis(),
                    error_kind = classify_reqwest_error(&error),
                    error = %safe_reqwest_error(&error),
                    "upstream stream error"
                );
                write_raw_event(
                    &self.raw_log,
                    "upstream_stream_error",
                    json!({
                        "local_id": &self.local_id,
                        "target": self.target.as_str(),
                        "status": self.status,
                        "chunk_count": self.chunk_count,
                        "byte_count": self.byte_count,
                        "elapsed_ms": elapsed.as_millis(),
                        "error_kind": classify_reqwest_error(&error),
                        "error": safe_reqwest_error(&error),
                    }),
                );
                Poll::Ready(Some(Err(std::io::Error::other(error))))
            }
            Poll::Ready(None) => {
                self.completed = true;
                let elapsed = self.started.elapsed();
                info!(
                    local_id = %self.local_id,
                    target = self.target.as_str(),
                    status = self.status,
                    chunk_count = self.chunk_count,
                    byte_count = self.byte_count,
                    elapsed_ms = elapsed.as_millis(),
                    "upstream stream completed"
                );
                write_raw_event(
                    &self.raw_log,
                    "upstream_stream_completed",
                    json!({
                        "local_id": &self.local_id,
                        "target": self.target.as_str(),
                        "status": self.status,
                        "chunk_count": self.chunk_count,
                        "byte_count": self.byte_count,
                        "elapsed_ms": elapsed.as_millis(),
                    }),
                );
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for LoggedByteStream<S> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let elapsed = self.started.elapsed();
        warn!(
            local_id = %self.local_id,
            target = self.target.as_str(),
            status = self.status,
            chunk_count = self.chunk_count,
            byte_count = self.byte_count,
            elapsed_ms = elapsed.as_millis(),
            "upstream stream dropped before completion"
        );
        write_raw_event(
            &self.raw_log,
            "upstream_stream_dropped",
            json!({
                "local_id": &self.local_id,
                "target": self.target.as_str(),
                "status": self.status,
                "chunk_count": self.chunk_count,
                "byte_count": self.byte_count,
                "elapsed_ms": elapsed.as_millis(),
            }),
        );
    }
}

impl<S> Unpin for LoggedByteStream<S> {}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP_RESPONSE_HEADERS
        .iter()
        .any(|candidate| name.as_str().eq_ignore_ascii_case(candidate))
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let text = serde_json::to_vec(&body).expect("serializing static JSON response should not fail");
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(text))
        .expect("building static JSON response should not fail")
}

fn header_value(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| truncate_for_log(value, 96))
}

fn classify_reqwest_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_request() {
        "request"
    } else {
        "unknown"
    }
}

fn safe_reqwest_error(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), &redact_url(url.as_str()));
    }
    truncate_for_log(&text, 256)
}

fn auth_error_class(error: &AuthError) -> &'static str {
    match error {
        AuthError::Missing => "missing",
        AuthError::Expired { .. } => "expired",
        AuthError::ExpiredMissingGithubToken { .. } => "expired_missing_github_token",
        AuthError::ReadTokenFile { .. } => "read_token_file",
        AuthError::ParseTokenFile { .. } => "parse_token_file",
        AuthError::MissingCopilotToken { .. } => "missing_copilot_token",
        AuthError::RefreshStatus { .. } => "refresh_status",
        AuthError::RefreshRequest { .. } => "refresh_request",
        AuthError::WriteTokenFile { .. } => "write_token_file",
        AuthError::MissingRefreshFields => "missing_refresh_fields",
        AuthError::InvalidIncomingAuthorization => "invalid_incoming_authorization",
    }
}

#[derive(Clone, Copy)]
enum Target {
    Models,
    Responses,
}

impl Target {
    fn as_str(self) -> &'static str {
        match self {
            Self::Models => "Models",
            Self::Responses => "Responses",
        }
    }

    fn url(self, config: &AppConfig) -> &str {
        match self {
            Self::Models => &config.upstream_models_url,
            Self::Responses => &config.upstream_responses_url,
        }
    }
}
