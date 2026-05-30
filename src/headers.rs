use crate::config::CopilotHeaderConfig;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, USER_AGENT,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use thiserror::Error;

pub const FORWARDED_CODEX_HEADERS: &[&str] = &[
    "x-client-request-id",
    "x-codex-parent-thread-id",
    "x-codex-sandbox",
    "x-codex-window-id",
    "x-openai-subagent",
];

pub fn forwarded_codex_header_names(inbound: &HeaderMap) -> Vec<&'static str> {
    FORWARDED_CODEX_HEADERS
        .iter()
        .copied()
        .filter(|header| inbound.contains_key(*header))
        .collect()
}

#[derive(Debug, Error)]
pub enum HeaderBuildError {
    #[error("invalid upstream authorization header")]
    InvalidAuthorization,
    #[error("invalid static upstream header value for {0}")]
    InvalidStaticValue(&'static str),
}

pub fn build_upstream_headers(
    inbound: &HeaderMap,
    authorization: &str,
    options: &CopilotHeaderConfig,
    default_accept: &'static str,
    request_id: &str,
    default_content_type: bool,
) -> Result<HeaderMap, HeaderBuildError> {
    let mut out = HeaderMap::new();

    let mut auth =
        HeaderValue::from_str(authorization).map_err(|_| HeaderBuildError::InvalidAuthorization)?;
    auth.set_sensitive(true);
    out.insert(AUTHORIZATION, auth);

    if let Some(value) = inbound.get(CONTENT_TYPE) {
        out.insert(CONTENT_TYPE, value.clone());
    } else if default_content_type {
        out.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }

    if let Some(value) = inbound.get(ACCEPT) {
        out.insert(ACCEPT, value.clone());
    } else {
        insert_static(&mut out, ACCEPT, default_accept, "accept")?;
    }

    copy_if_present(inbound, &mut out, ACCEPT_ENCODING);
    copy_if_present(inbound, &mut out, CONTENT_ENCODING);

    for header in FORWARDED_CODEX_HEADERS {
        let name = HeaderName::from_static(header);
        copy_if_present(inbound, &mut out, name);
    }

    insert_static(
        &mut out,
        HeaderName::from_static("copilot-integration-id"),
        "vscode-chat",
        "copilot-integration-id",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("editor-plugin-version"),
        format!("copilot-chat/{}", options.copilot_chat_version),
        "editor-plugin-version",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("editor-version"),
        options.copilot_editor_version.clone(),
        "editor-version",
    )?;
    insert_owned(
        &mut out,
        USER_AGENT,
        format!("GitHubCopilotChat/{}", options.copilot_chat_version),
        "user-agent",
    )?;
    insert_static(
        &mut out,
        HeaderName::from_static("openai-intent"),
        "conversation-agent",
        "openai-intent",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("x-github-api-version"),
        options.github_api_version.clone(),
        "x-github-api-version",
    )?;
    insert_static(
        &mut out,
        HeaderName::from_static("x-initiator"),
        "agent",
        "x-initiator",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("x-request-id"),
        request_id.to_owned(),
        "x-request-id",
    )?;
    insert_static(
        &mut out,
        HeaderName::from_static("x-vscode-user-agent-library-version"),
        "electron-fetch",
        "x-vscode-user-agent-library-version",
    )?;

    Ok(out)
}

fn copy_if_present(inbound: &HeaderMap, out: &mut HeaderMap, name: HeaderName) {
    if let Some(value) = inbound.get(&name) {
        out.insert(name, value.clone());
    }
}

fn insert_static(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &'static str,
    label: &'static str,
) -> Result<(), HeaderBuildError> {
    let value =
        HeaderValue::from_str(value).map_err(|_| HeaderBuildError::InvalidStaticValue(label))?;
    headers.insert(name, value);
    Ok(())
}

fn insert_owned(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: String,
    label: &'static str,
) -> Result<(), HeaderBuildError> {
    let value =
        HeaderValue::from_str(&value).map_err(|_| HeaderBuildError::InvalidStaticValue(label))?;
    headers.insert(name, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redaction::redact_headers;
    use http::header::CONTENT_TYPE;

    fn options() -> CopilotHeaderConfig {
        CopilotHeaderConfig {
            copilot_chat_version: "test-chat".to_owned(),
            copilot_editor_version: "vscode/test".to_owned(),
            github_api_version: "2025-10-01".to_owned(),
        }
    }

    #[test]
    fn forwards_codex_headers_and_injects_copilot_headers() {
        let mut inbound = HeaderMap::new();
        inbound.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        inbound.insert(
            HeaderName::from_static("x-codex-window-id"),
            HeaderValue::from_static("thread:0"),
        );

        let headers = build_upstream_headers(
            &inbound,
            "Bearer secret-token",
            &options(),
            "text/event-stream",
            "fixed-request-id",
            true,
        )
        .unwrap();

        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer secret-token");
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(headers.get("x-codex-window-id").unwrap(), "thread:0");
        assert_eq!(
            headers.get("copilot-integration-id").unwrap(),
            "vscode-chat"
        );
        assert_eq!(
            headers.get("editor-plugin-version").unwrap(),
            "copilot-chat/test-chat"
        );
        assert_eq!(headers.get("editor-version").unwrap(), "vscode/test");
        assert_eq!(headers.get("x-github-api-version").unwrap(), "2025-10-01");
        assert_eq!(headers.get("x-request-id").unwrap(), "fixed-request-id");

        let redacted = redact_headers(&headers);
        assert!(!format!("{redacted:?}").contains("secret-token"));
    }

    #[test]
    fn responses_requests_default_to_json_bodies_and_sse_responses_when_codex_is_silent() {
        let inbound = HeaderMap::new();

        let headers = build_upstream_headers(
            &inbound,
            "Bearer secret-token",
            &options(),
            "text/event-stream",
            "fixed-request-id",
            true,
        )
        .unwrap();

        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap(),
            "application/json",
            "Responses requests should default to the JSON wire format Codex sends."
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap(),
            "text/event-stream",
            "Responses requests should default to accepting native Responses SSE."
        );
    }

    #[test]
    fn models_requests_do_not_invent_a_body_content_type() {
        let inbound = HeaderMap::new();

        let headers = build_upstream_headers(
            &inbound,
            "Bearer secret-token",
            &options(),
            "application/json",
            "fixed-request-id",
            false,
        )
        .unwrap();

        assert!(
            headers.get(CONTENT_TYPE).is_none(),
            "Bodyless model-catalog requests should not claim to send JSON bodies."
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap(),
            "application/json",
            "The model catalog boundary should ask for JSON when Codex omits Accept."
        );
    }
}
