use http::{HeaderMap, HeaderName};
use std::collections::BTreeMap;

const REDACTED: &str = "<redacted>";

pub fn is_sensitive_header(name: &HeaderName) -> bool {
    let lower = name.as_str().to_ascii_lowercase();
    lower == "authorization"
        || lower == "proxy-authorization"
        || lower == "cookie"
        || lower == "set-cookie"
        || lower.contains("token")
}

pub fn redact_header_value(name: &HeaderName, value: &str) -> String {
    if is_sensitive_header(name) {
        REDACTED.to_owned()
    } else {
        value.to_owned()
    }
}

pub fn redact_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value.to_str().unwrap_or("<non-utf8>");
            (name.as_str().to_owned(), redact_header_value(name, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{AUTHORIZATION, CONTENT_TYPE};
    use http::HeaderValue;

    #[test]
    fn redacts_sensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-token"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let redacted = redact_headers(&headers);

        assert_eq!(
            redacted.get("authorization"),
            Some(&"<redacted>".to_owned())
        );
        assert_eq!(
            redacted.get("content-type"),
            Some(&"application/json".to_owned())
        );
        assert!(!format!("{redacted:?}").contains("secret-token"));
    }
}
