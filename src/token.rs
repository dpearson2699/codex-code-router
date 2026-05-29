use crate::config::AuthConfig;
use http::header::AUTHORIZATION;
use http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;
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
    #[error("failed to read Copilot token file {path}: {source}")]
    ReadTokenFile { path: PathBuf, source: io::Error },
    #[error("failed to parse Copilot token file {path}: {source}")]
    ParseTokenFile {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("Copilot token file is missing copilotToken: {path}")]
    MissingCopilotToken { path: PathBuf },
    #[error("incoming Authorization header is not valid UTF-8")]
    InvalidIncomingAuthorization,
}

#[derive(Debug, Deserialize)]
struct CopilotTokenFile {
    #[serde(rename = "copilotToken")]
    copilot_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<Value>,
}

pub fn resolve_upstream_authorization(
    auth: &AuthConfig,
    inbound: &HeaderMap,
) -> Result<String, AuthError> {
    let configured = configured_token(auth);
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

pub fn printable_token_from_auth_config(auth: &AuthConfig) -> Result<String, AuthError> {
    configured_token(auth)?.ok_or(AuthError::Missing)
}

fn configured_token(auth: &AuthConfig) -> Result<Option<String>, AuthError> {
    if let Some(token) = auth
        .bearer_token
        .as_ref()
        .map(|value| strip_bearer_prefix(value))
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(token));
    }

    load_token_file(&auth.token_file, auth.token_expiry_buffer)
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

fn load_token_file(path: &Path, expiry_buffer: Duration) -> Result<Option<String>, AuthError> {
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

    ensure_not_expired(path, data.expires_at.as_ref(), expiry_buffer)?;

    let token = data
        .copilot_token
        .map(|value| strip_bearer_prefix(&value))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::MissingCopilotToken {
            path: path.to_path_buf(),
        })?;

    Ok(Some(token))
}

fn ensure_not_expired(
    path: &Path,
    expires_at: Option<&Value>,
    expiry_buffer: Duration,
) -> Result<(), AuthError> {
    let Some(expires_at) = expires_at.and_then(epoch_seconds) else {
        return Ok(());
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let buffer = expiry_buffer.as_secs();

    if now >= expires_at.saturating_sub(buffer) {
        return Err(AuthError::Expired {
            path: path.to_path_buf(),
        });
    }

    Ok(())
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
    use http::HeaderValue;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::NamedTempFile;

    fn auth_config(token_file: PathBuf) -> AuthConfig {
        AuthConfig {
            bearer_token: None,
            token_file,
            token_expiry_buffer: Duration::from_secs(300),
        }
    }

    #[test]
    fn reads_copilot_token_file_for_printing() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), r#"{"copilotToken":"secret-token"}"#).unwrap();

        let token =
            printable_token_from_auth_config(&auth_config(file.path().to_path_buf())).unwrap();

        assert_eq!(token, "secret-token");
    }

    #[test]
    fn token_file_expiry_errors_do_not_include_secret() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"secret-token","expiresAt":1}"#,
        )
        .unwrap();

        let error =
            printable_token_from_auth_config(&auth_config(file.path().to_path_buf())).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("expired"));
        assert!(!message.contains("secret-token"));
    }

    #[test]
    fn env_token_wins_and_is_normalized() {
        let auth = AuthConfig {
            bearer_token: Some("Bearer secret-token".to_owned()),
            token_file: PathBuf::from("/definitely/not/present"),
            token_expiry_buffer: Duration::from_secs(300),
        };

        let headers = HeaderMap::new();
        let auth_header = resolve_upstream_authorization(&auth, &headers).unwrap();

        assert_eq!(auth_header, "Bearer secret-token");
        assert_eq!(
            printable_token_from_auth_config(&auth).unwrap(),
            "secret-token"
        );
    }

    #[test]
    fn incoming_authorization_is_used_when_service_token_is_absent() {
        let auth = auth_config(PathBuf::from("/definitely/not/present"));
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer incoming-token"),
        );

        let auth_header = resolve_upstream_authorization(&auth, &headers).unwrap();

        assert_eq!(auth_header, "Bearer incoming-token");
    }

    #[test]
    fn token_file_takes_precedence_over_incoming_authorization() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), r#"{"copilotToken":"file-token"}"#).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer incoming-token"),
        );

        let auth_header =
            resolve_upstream_authorization(&auth_config(file.path().to_path_buf()), &headers)
                .unwrap();

        assert_eq!(
            auth_header, "Bearer file-token",
            "Service-owned token files should win over client-supplied local Authorization."
        );
    }

    #[test]
    fn accepts_future_expires_at() {
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

        let token =
            printable_token_from_auth_config(&auth_config(file.path().to_path_buf())).unwrap();

        assert_eq!(token, "secret-token");
    }

    #[test]
    fn accepts_future_expires_at_in_epoch_milliseconds() {
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

        let token =
            printable_token_from_auth_config(&auth_config(file.path().to_path_buf())).unwrap();

        assert_eq!(
            token, "secret-token",
            "Existing Copilot token files may store expiresAt as epoch milliseconds."
        );
    }

    #[test]
    fn malformed_token_file_errors_do_not_echo_file_contents() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"secret-token-that-must-not-leak","expiresAt":"not closed""#,
        )
        .unwrap();

        let error =
            printable_token_from_auth_config(&auth_config(file.path().to_path_buf())).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("failed to parse"));
        assert!(
            !message.contains("secret-token-that-must-not-leak"),
            "Parse diagnostics should identify the bad token file without echoing secret-bearing contents."
        );
    }
}
