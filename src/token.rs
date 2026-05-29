use crate::config::{AuthConfig, CopilotHeaderConfig};
use http::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing Copilot bearer token; set COPILOT_BEARER_TOKEN, provide a valid COPILOT_TOKEN_FILE, or send Authorization from Codex provider auth")]
    Missing,
    #[error("Copilot token file is expired or near expiry: {path}")]
    Expired { path: PathBuf },
    #[error("Copilot token file is expired or near expiry and cannot be refreshed because it is missing githubToken: {path}")]
    ExpiredMissingGithubToken { path: PathBuf },
    #[error("failed to read Copilot token file {path}: {source}")]
    ReadTokenFile { path: PathBuf, source: io::Error },
    #[error("failed to parse Copilot token file {path}: {source}")]
    ParseTokenFile {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("Copilot token file is missing copilotToken: {path}")]
    MissingCopilotToken { path: PathBuf },
    #[error("failed to refresh Copilot token: HTTP {status}")]
    RefreshStatus { status: reqwest::StatusCode },
    #[error("failed to refresh Copilot token: {source}")]
    RefreshRequest { source: reqwest::Error },
    #[error("failed to write refreshed Copilot token file {path}: {source}")]
    WriteTokenFile { path: PathBuf, source: io::Error },
    #[error("Copilot refresh response is missing token or expires_at")]
    MissingRefreshFields,
    #[error("incoming Authorization header is not valid UTF-8")]
    InvalidIncomingAuthorization,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CopilotTokenFile {
    #[serde(rename = "githubToken", skip_serializing_if = "Option::is_none")]
    github_token: Option<String>,
    #[serde(rename = "copilotToken")]
    copilot_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<Value>,
    #[serde(rename = "lastUpdated", skip_serializing_if = "Option::is_none")]
    last_updated: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct CopilotRefreshResponse {
    token: Option<String>,
    expires_at: Option<u64>,
    endpoints: Option<CopilotEndpoints>,
}

#[derive(Debug, Deserialize)]
struct CopilotEndpoints {
    api: Option<String>,
}

struct RefreshedCopilotToken {
    token: String,
    expires_at: u64,
    endpoint: Option<String>,
}

pub async fn resolve_upstream_authorization(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
    inbound: &HeaderMap,
) -> Result<String, AuthError> {
    let configured = configured_token(auth, headers, client).await;
    if let Ok(Some(token)) = configured.as_ref() {
        return Ok(as_authorization_header(token));
    }

    if let Some(incoming) = incoming_authorization(inbound)? {
        return Ok(incoming);
    }

    match configured {
        Err(error) => Err(error),
        Ok(None) => Err(AuthError::Missing),
        Ok(Some(_)) => unreachable!("configured token is returned above"),
    }
}

pub async fn printable_token_from_auth_config(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<String, AuthError> {
    configured_token(auth, headers, client)
        .await?
        .ok_or(AuthError::Missing)
}

async fn configured_token(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<Option<String>, AuthError> {
    if let Some(token) = auth
        .bearer_token
        .as_ref()
        .map(|value| strip_bearer_prefix(value))
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(token));
    }

    load_token_file(auth, headers, client).await
}

fn incoming_authorization(inbound: &HeaderMap) -> Result<Option<String>, AuthError> {
    inbound
        .get(AUTHORIZATION)
        .map(|value| {
            value
                .to_str()
                .map(|raw| raw.trim().to_owned())
                .map_err(|_| AuthError::InvalidIncomingAuthorization)
        })
        .transpose()
        .map(|value| value.filter(|raw| !raw.is_empty()))
}

fn as_authorization_header(token: &str) -> String {
    format!("Bearer {}", strip_bearer_prefix(token))
}

fn strip_bearer_prefix(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed
        .get(..7)
        .map(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        .unwrap_or(false)
    {
        trimmed[7..].trim().to_owned()
    } else {
        trimmed.to_owned()
    }
}

async fn load_token_file(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<Option<String>, AuthError> {
    let path = &auth.token_file;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AuthError::ReadTokenFile {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let data: CopilotTokenFile =
        serde_json::from_str(&text).map_err(|source| AuthError::ParseTokenFile {
            path: path.to_path_buf(),
            source,
        })?;

    if !is_expired_or_near_expiry(data.expires_at.as_ref(), auth.token_expiry_buffer) {
        return copilot_token_from_data(path, data);
    }

    if !auth.refresh_enabled {
        return Err(AuthError::Expired {
            path: path.to_path_buf(),
        });
    }

    let github_token = data
        .github_token
        .as_deref()
        .map(strip_bearer_prefix)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::ExpiredMissingGithubToken {
            path: path.to_path_buf(),
        })?;

    let refreshed = refresh_copilot_token(auth, headers, client, &github_token).await?;
    let token = refreshed.token.clone();
    write_refreshed_token_file(path, data, refreshed)?;

    Ok(Some(token))
}

fn copilot_token_from_data(
    path: &Path,
    data: CopilotTokenFile,
) -> Result<Option<String>, AuthError> {
    let token = data
        .copilot_token
        .map(|value| strip_bearer_prefix(&value))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::MissingCopilotToken {
            path: path.to_path_buf(),
        })?;

    Ok(Some(token))
}

fn is_expired_or_near_expiry(expires_at: Option<&Value>, expiry_buffer: Duration) -> bool {
    let Some(expires_at) = expires_at.and_then(epoch_seconds) else {
        return false;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let buffer = expiry_buffer.as_secs();

    now >= expires_at.saturating_sub(buffer)
}

async fn refresh_copilot_token(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
    github_token: &str,
) -> Result<RefreshedCopilotToken, AuthError> {
    let mut authorization = HeaderValue::from_str(&format!("Bearer {github_token}"))
        .map_err(|_| AuthError::MissingRefreshFields)?;
    authorization.set_sensitive(true);

    let response = client
        .get(&auth.copilot_token_url)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(AUTHORIZATION, authorization)
        .header(
            USER_AGENT,
            header_value(
                format!("GitHubCopilotChat/{}", headers.copilot_chat_version),
                "user-agent",
            )?,
        )
        .header(
            HeaderName::from_static("editor-version"),
            header_value(headers.copilot_editor_version.clone(), "editor-version")?,
        )
        .header(
            HeaderName::from_static("editor-plugin-version"),
            header_value(
                format!("copilot-chat/{}", headers.copilot_chat_version),
                "editor-plugin-version",
            )?,
        )
        .send()
        .await
        .map_err(|source| AuthError::RefreshRequest { source })?;

    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::RefreshStatus { status });
    }

    let data = response
        .json::<CopilotRefreshResponse>()
        .await
        .map_err(|source| AuthError::RefreshRequest { source })?;

    let token = data
        .token
        .map(|value| strip_bearer_prefix(&value))
        .filter(|value| !value.is_empty())
        .ok_or(AuthError::MissingRefreshFields)?;
    let expires_at = data.expires_at.ok_or(AuthError::MissingRefreshFields)?;
    let endpoint = data
        .endpoints
        .and_then(|endpoints| endpoints.api)
        .map(|api| format!("{}/chat/completions", api.trim_end_matches('/')));

    Ok(RefreshedCopilotToken {
        token,
        expires_at,
        endpoint,
    })
}

fn write_refreshed_token_file(
    path: &Path,
    mut data: CopilotTokenFile,
    refreshed: RefreshedCopilotToken,
) -> Result<(), AuthError> {
    data.copilot_token = Some(refreshed.token);
    data.expires_at = Some(Value::from(refreshed.expires_at));
    if let Some(endpoint) = refreshed.endpoint {
        data.endpoint = Some(endpoint);
    }
    data.last_updated = Some(Value::from(now_epoch_seconds()));

    let text = serde_json::to_string_pretty(&data).expect("token file should serialize");
    fs::write(path, format!("{text}\n")).map_err(|source| AuthError::WriteTokenFile {
        path: path.to_path_buf(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|source| AuthError::WriteTokenFile {
            path: path.to_path_buf(),
            source,
        })?;
    }

    Ok(())
}

fn header_value(value: String, _label: &'static str) -> Result<HeaderValue, AuthError> {
    HeaderValue::from_str(&value).map_err(|_| AuthError::MissingRefreshFields)
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn epoch_seconds(value: &Value) -> Option<u64> {
    let raw = match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse::<u64>().ok(),
        _ => None,
    }?;

    if raw > 10_000_000_000 {
        Some(raw / 1_000)
    } else {
        Some(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_COPILOT_TOKEN_URL;
    use axum::routing::get;
    use axum::{Json, Router};
    use http::{HeaderValue, StatusCode};
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::NamedTempFile;
    use tokio::net::TcpListener;

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

    fn auth_config(token_file: PathBuf) -> AuthConfig {
        AuthConfig {
            bearer_token: None,
            token_file,
            token_expiry_buffer: Duration::from_secs(300),
            refresh_enabled: true,
            copilot_token_url: DEFAULT_COPILOT_TOKEN_URL.to_owned(),
        }
    }

    fn header_config() -> CopilotHeaderConfig {
        CopilotHeaderConfig {
            copilot_chat_version: "test-chat".to_owned(),
            copilot_editor_version: "vscode/test".to_owned(),
            github_api_version: "2025-10-01".to_owned(),
        }
    }

    async fn printable_token(auth: &AuthConfig) -> Result<String, AuthError> {
        let headers = header_config();
        let client = reqwest::Client::new();
        printable_token_from_auth_config(auth, &headers, &client).await
    }

    async fn resolved_authorization(
        auth: &AuthConfig,
        inbound: &HeaderMap,
    ) -> Result<String, AuthError> {
        let headers = header_config();
        let client = reqwest::Client::new();
        resolve_upstream_authorization(auth, &headers, &client, inbound).await
    }

    #[tokio::test]
    async fn reads_copilot_token_file_for_printing() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), r#"{"copilotToken":"secret-token"}"#).unwrap();

        let token = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(token, "secret-token");
    }

    #[tokio::test]
    async fn token_file_expiry_errors_do_not_include_secret() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"secret-token","expiresAt":1}"#,
        )
        .unwrap();

        let error = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("expired"));
        assert!(!message.contains("secret-token"));
    }

    #[tokio::test]
    async fn env_token_wins_and_is_normalized() {
        let auth = AuthConfig {
            bearer_token: Some("Bearer secret-token".to_owned()),
            token_file: PathBuf::from("/definitely/not/present"),
            token_expiry_buffer: Duration::from_secs(300),
            refresh_enabled: true,
            copilot_token_url: DEFAULT_COPILOT_TOKEN_URL.to_owned(),
        };

        let headers = HeaderMap::new();
        let auth_header = resolved_authorization(&auth, &headers).await.unwrap();

        assert_eq!(auth_header, "Bearer secret-token");
        assert_eq!(printable_token(&auth).await.unwrap(), "secret-token");
    }

    #[tokio::test]
    async fn incoming_authorization_is_used_when_service_token_is_absent() {
        let auth = auth_config(PathBuf::from("/definitely/not/present"));
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer incoming-token"),
        );

        let auth_header = resolved_authorization(&auth, &headers).await.unwrap();

        assert_eq!(auth_header, "Bearer incoming-token");
    }

    #[tokio::test]
    async fn token_file_takes_precedence_over_incoming_authorization() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), r#"{"copilotToken":"file-token"}"#).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer incoming-token"),
        );

        let auth_header = resolved_authorization(&auth_config(file.path().to_path_buf()), &headers)
            .await
            .unwrap();

        assert_eq!(
            auth_header, "Bearer file-token",
            "Service-owned token files should win over client-supplied local Authorization."
        );
    }

    #[tokio::test]
    async fn accepts_future_expires_at() {
        let file = NamedTempFile::new().unwrap();
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        fs::write(
            file.path(),
            format!(r#"{{"copilotToken":"secret-token","expiresAt":{future}}}"#),
        )
        .unwrap();

        let token = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(token, "secret-token");
    }

    #[tokio::test]
    async fn accepts_future_expires_at_in_epoch_milliseconds() {
        let file = NamedTempFile::new().unwrap();
        let future_ms = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600)
            * 1_000;
        fs::write(
            file.path(),
            format!(r#"{{"copilotToken":"secret-token","expiresAt":{future_ms}}}"#),
        )
        .unwrap();

        let token = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(
            token, "secret-token",
            "Existing Copilot token files may store expiresAt as epoch milliseconds."
        );
    }

    #[tokio::test]
    async fn malformed_token_file_errors_do_not_echo_file_contents() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"secret-token-that-must-not-leak","expiresAt":"not closed""#,
        )
        .unwrap();

        let error = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("failed to parse"));
        assert!(
            !message.contains("secret-token-that-must-not-leak"),
            "Parse diagnostics should identify the bad token file without echoing secret-bearing contents."
        );
    }

    #[tokio::test]
    async fn expired_token_file_refreshes_from_saved_github_token() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"githubToken":"github-secret","copilotToken":"stale-copilot-secret","expiresAt":1}"#,
        )
        .unwrap();
        let recorded = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let future = now_epoch_seconds() + 3_600;
        let server = spawn_router({
            let recorded = recorded.clone();
            Router::new().route(
                "/copilot_internal/v2/token",
                get(move |headers: HeaderMap| {
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push(headers);
                        Json(json!({
                            "token": "fresh-copilot-token",
                            "expires_at": future,
                            "endpoints": {"api": "https://api.githubcopilot.test"}
                        }))
                    }
                }),
            )
        })
        .await;
        let mut auth = auth_config(file.path().to_path_buf());
        auth.copilot_token_url = server.url("/copilot_internal/v2/token");

        let token = printable_token(&auth).await.unwrap();

        assert_eq!(token, "fresh-copilot-token");
        let saved: Value = serde_json::from_str(&fs::read_to_string(file.path()).unwrap()).unwrap();
        assert_eq!(saved["copilotToken"], "fresh-copilot-token");
        assert_eq!(saved["githubToken"], "github-secret");
        assert_eq!(saved["expiresAt"], future);
        assert_eq!(
            saved["endpoint"],
            "https://api.githubcopilot.test/chat/completions"
        );
        assert!(
            !fs::read_to_string(file.path())
                .unwrap()
                .contains("stale-copilot-secret"),
            "Refreshing should replace the expired Copilot session token rather than keeping stale token material."
        );

        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].get(AUTHORIZATION).unwrap(),
            "Bearer github-secret"
        );
        assert_eq!(
            requests[0].get(USER_AGENT).unwrap(),
            "GitHubCopilotChat/test-chat"
        );
        assert_eq!(requests[0].get("editor-version").unwrap(), "vscode/test");
        assert_eq!(
            requests[0].get("editor-plugin-version").unwrap(),
            "copilot-chat/test-chat"
        );
    }

    #[tokio::test]
    async fn refresh_http_errors_do_not_echo_saved_tokens() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"githubToken":"github-secret-that-must-not-leak","copilotToken":"stale-secret-that-must-not-leak","expiresAt":1}"#,
        )
        .unwrap();
        let server = spawn_router(Router::new().route(
            "/copilot_internal/v2/token",
            get(|| async { (StatusCode::FORBIDDEN, "forbidden") }),
        ))
        .await;
        let mut auth = auth_config(file.path().to_path_buf());
        auth.copilot_token_url = server.url("/copilot_internal/v2/token");

        let error = printable_token(&auth).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("HTTP 403"));
        assert!(!message.contains("github-secret-that-must-not-leak"));
        assert!(!message.contains("stale-secret-that-must-not-leak"));
    }
}
