use crate::config::AppConfig;
use crate::headers::build_upstream_headers;
use crate::retry::{retry_budget_exceeded, select_retry_wait};
use crate::token::{resolve_upstream_authorization, AuthError};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;
use tracing::{info, warn};
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
    let upstream = config.upstream_responses_url.clone();
    let state = AppState::new(config)?;

    info!(%addr, upstream_responses_url = %upstream, "codex-code-router listening");

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
    let authorization = match resolve_upstream_authorization(&state.config.auth, &inbound_headers) {
        Ok(authorization) => authorization,
        Err(error) => return auth_error_response(error),
    };

    let url = target.url(&state.config).to_owned();
    let mut attempt = 0_u32;
    let mut total_wait = Duration::ZERO;

    loop {
        let request_id = Uuid::new_v4().to_string();
        let upstream_headers = match build_upstream_headers(
            &inbound_headers,
            &authorization,
            &state.config.headers,
            default_accept,
            &request_id,
            default_content_type,
        ) {
            Ok(headers) => headers,
            Err(error) => {
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "error": "upstream_header_build_failed",
                        "message": error.to_string(),
                    }),
                )
            }
        };

        let mut request = state
            .client
            .request(method.clone(), &url)
            .headers(upstream_headers);

        if let Some(body) = body.clone() {
            request = request.body(body);
        }

        let upstream = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "error": "upstream_request_failed",
                        "message": error.to_string(),
                    }),
                )
            }
        };

        if upstream.status() != StatusCode::TOO_MANY_REQUESTS {
            return response_from_upstream(upstream);
        }

        let wait = select_retry_wait(
            upstream.headers(),
            attempt,
            &state.config.rate_limit,
            SystemTime::now(),
        );
        if retry_budget_exceeded(
            total_wait,
            wait.delay,
            state.config.rate_limit.max_total_wait,
        ) {
            return response_from_upstream(upstream);
        }

        attempt = attempt.saturating_add(1);
        warn!(
            attempt,
            wait_ms = wait.delay.as_millis(),
            wait_source = ?wait.source,
            total_wait_ms = total_wait.as_millis(),
            %request_id,
            "upstream rate-limited request; retrying after delay"
        );
        tokio::time::sleep(wait.delay).await;
        total_wait = total_wait.saturating_add(wait.delay);
    }
}

fn auth_error_response(error: AuthError) -> Response {
    let status = match &error {
        AuthError::Missing
        | AuthError::Expired { .. }
        | AuthError::InvalidIncomingAuthorization => StatusCode::UNAUTHORIZED,
        AuthError::ReadTokenFile { .. }
        | AuthError::ParseTokenFile { .. }
        | AuthError::MissingCopilotToken { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };

    json_response(
        status,
        json!({
            "error": "copilot_auth_unavailable",
            "message": error.to_string(),
        }),
    )
}

fn response_from_upstream(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let mut builder = Response::builder().status(status);

    for (name, value) in upstream.headers() {
        if !is_hop_by_hop(name) {
            builder = builder.header(name.clone(), value.clone());
        }
    }

    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
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

#[derive(Clone, Copy)]
enum Target {
    Models,
    Responses,
}

impl Target {
    fn url(self, config: &AppConfig) -> &str {
        match self {
            Self::Models => &config.upstream_models_url,
            Self::Responses => &config.upstream_responses_url,
        }
    }
}
