use crate::config::RawLogConfig;
use crate::redaction::{redact_json, truncate_json_strings};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

const RAW_LOG_STRING_MAX_CHARS: usize = 512;

pub fn write_raw_event(config: &RawLogConfig, kind: &'static str, fields: Value) {
    if !config.enabled {
        return;
    }

    if let Err(error) = write_raw_event_inner(config, kind, fields) {
        warn!(kind, error = %error, "failed to write raw diagnostic event");
    }
}

fn write_raw_event_inner(
    config: &RawLogConfig,
    kind: &'static str,
    fields: Value,
) -> std::io::Result<()> {
    if let Some(parent) = config.file.parent() {
        fs::create_dir_all(parent)?;
    }

    let event = json!({
        "timestamp_unix_ms": unix_millis(),
        "kind": kind,
        "fields": fields,
    });
    let event = truncate_json_strings(&redact_json(&event), RAW_LOG_STRING_MAX_CHARS);
    let mut line = serde_json::to_string(&event).expect("raw diagnostic event should serialize");

    if line.len() > config.max_bytes {
        line = serde_json::to_string(&json!({
            "timestamp_unix_ms": unix_millis(),
            "kind": "diagnostic_event_truncated",
            "fields": {
                "original_kind": kind,
                "max_bytes": config.max_bytes,
                "original_bytes": line.len(),
            }
        }))
        .expect("raw diagnostic truncation event should serialize");
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.file)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn raw_events_are_redacted_and_bounded() {
        let file = NamedTempFile::new().unwrap();
        let config = RawLogConfig {
            enabled: true,
            file: file.path().to_path_buf(),
            max_bytes: 4096,
        };

        write_raw_event(
            &config,
            "test_event",
            json!({
                "authorization": "Bearer secret-token",
                "body_preview": "not a body capture",
                "safe": "visible",
            }),
        );

        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("test_event"));
        assert!(text.contains("visible"));
        assert!(!text.contains("secret-token"));
    }
}
