use std::env;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 60001;
pub const DEFAULT_RESPONSES_URL: &str = "https://api.githubcopilot.com/responses";
pub const DEFAULT_MODELS_URL: &str = "https://api.githubcopilot.com/models";
pub const DEFAULT_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub const DEFAULT_GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const DEFAULT_GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const DEFAULT_GITHUB_OAUTH_CLIENT_ID: &str = "01ab8ac9400c4e429b23";
pub const DEFAULT_GITHUB_OAUTH_SCOPE: &str = "read:user";

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub upstream_responses_url: String,
    pub upstream_models_url: String,
    pub request_timeout: Duration,
    pub headers: CopilotHeaderConfig,
    pub auth: AuthConfig,
    pub rate_limit: RateLimitConfig,
}

#[derive(Clone, Debug)]
pub struct CopilotHeaderConfig {
    pub copilot_chat_version: String,
    pub copilot_editor_version: String,
    pub github_api_version: String,
}

#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub bearer_token: Option<String>,
    pub token_file: PathBuf,
    pub token_expiry_buffer: Duration,
    pub refresh_enabled: bool,
    pub copilot_token_url: String,
    pub github_device_code_url: String,
    pub github_access_token_url: String,
    pub github_oauth_client_id: String,
    pub github_oauth_scope: String,
}

#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    pub max_total_wait: Option<Duration>,
    pub max_sleep: Duration,
    pub initial_backoff: Duration,
    pub backoff_multiplier: f64,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self {
            host: read_string("HOST", DEFAULT_HOST),
            port: read_u16("PORT", DEFAULT_PORT),
            upstream_responses_url: read_string("COPILOT_RESPONSES_URL", DEFAULT_RESPONSES_URL),
            upstream_models_url: read_string("COPILOT_MODELS_URL", DEFAULT_MODELS_URL),
            request_timeout: read_duration_ms("REQUEST_TIMEOUT_MS", 300_000),
            headers: CopilotHeaderConfig {
                copilot_chat_version: read_string("COPILOT_CHAT_VERSION", "0.35.0"),
                copilot_editor_version: read_string("COPILOT_EDITOR_VERSION", "vscode/1.109.2"),
                github_api_version: read_string("GITHUB_API_VERSION", "2025-10-01"),
            },
            auth: AuthConfig {
                bearer_token: read_optional_string("COPILOT_BEARER_TOKEN"),
                token_file: read_token_file_path(),
                token_expiry_buffer: read_duration_seconds(
                    "COPILOT_TOKEN_EXPIRY_BUFFER_SECONDS",
                    300,
                ),
                refresh_enabled: read_bool("COPILOT_TOKEN_REFRESH", true),
                copilot_token_url: read_string("COPILOT_TOKEN_URL", DEFAULT_COPILOT_TOKEN_URL),
                github_device_code_url: read_string(
                    "GITHUB_DEVICE_CODE_URL",
                    DEFAULT_GITHUB_DEVICE_CODE_URL,
                ),
                github_access_token_url: read_string(
                    "GITHUB_ACCESS_TOKEN_URL",
                    DEFAULT_GITHUB_ACCESS_TOKEN_URL,
                ),
                github_oauth_client_id: read_string(
                    "GITHUB_OAUTH_CLIENT_ID",
                    DEFAULT_GITHUB_OAUTH_CLIENT_ID,
                ),
                github_oauth_scope: read_string("GITHUB_OAUTH_SCOPE", DEFAULT_GITHUB_OAUTH_SCOPE),
            },
            rate_limit: RateLimitConfig {
                max_total_wait: read_max_total_wait(),
                max_sleep: read_duration_ms("RATE_LIMIT_MAX_SLEEP_MS", 60_000),
                initial_backoff: read_duration_ms("RATE_LIMIT_INITIAL_BACKOFF_MS", 1_000),
                backoff_multiplier: read_f64("RATE_LIMIT_BACKOFF_MULTIPLIER", 2.0, 1.0),
            },
        }
    }
}

fn read_string(name: &str, fallback: &str) -> String {
    read_optional_string(name).unwrap_or_else(|| fallback.to_owned())
}

fn read_optional_string(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_u16(name: &str, fallback: u16) -> u16 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn read_duration_ms(name: &str, fallback_ms: u64) -> Duration {
    let value = env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback_ms);
    Duration::from_millis(value)
}

fn read_duration_seconds(name: &str, fallback_seconds: u64) -> Duration {
    let value = env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(fallback_seconds);
    Duration::from_secs(value)
}

fn read_max_total_wait() -> Option<Duration> {
    match env::var("RATE_LIMIT_MAX_TOTAL_WAIT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        Some(0) | None => None,
        Some(value) => Some(Duration::from_millis(value)),
    }
}

fn read_f64(name: &str, fallback: f64, minimum: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= minimum)
        .unwrap_or(fallback)
}

fn read_bool(name: &str, fallback: bool) -> bool {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .and_then(|value| match value.as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(fallback)
}

fn read_token_file_path() -> PathBuf {
    if let Some(path) = read_optional_string("COPILOT_TOKEN_FILE") {
        return PathBuf::from(path);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".copilot-tokens.json")
}
