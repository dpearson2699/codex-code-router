# codex-code-router

`codex-code-router` is a small Rust local HTTP service that lets Codex CLI use GitHub Copilot's Responses API endpoint through a local OpenAI-compatible Responses provider.

The current project decision is explicit: **custom Rust adapter, not LiteLLM, not Node, not a Codex launcher/wrapper**. Users run this service as a normal local/background process and switch Codex behavior with Codex profiles.

## What it does

- Exposes `GET /health`, `GET /v1/models`, and `POST /v1/responses` on `127.0.0.1:60001` by default.
- Forwards `GET /v1/models` to `https://api.githubcopilot.com/models` by default.
- Forwards `POST /v1/responses` to `https://api.githubcopilot.com/responses` by default.
- Injects Copilot/VS Code-style provider headers.
- Streams Responses SSE bytes back unchanged.
- Buffers each incoming request body only so HTTP `429` retries can replay the exact same bytes before anything is sent downstream.
- Retries upstream HTTP `429 Too Many Requests` using status/headers only.
- Redacts token-bearing headers in diagnostics helpers/tests.

## What it does not do

- No Chat Completions endpoint.
- No Anthropic Messages endpoint.
- No Anthropic or Chat Completions SSE conversion.
- No tool rewriting, MCP/namespaced tool rewriting, history rewriting, tool-output truncation, or CCR transformer behavior.
- No Docker/container workflow.
- No LiteLLM path for the current implementation.

Request bodies are forwarded as bytes by default. The adapter does **not** force `store: false`, remove `previous_response_id`, or mutate `include` unless a future concrete Copilot HTTP compatibility failure proves a targeted, isolated, tested mutation is necessary.

## One-command run

From this repo, this is the normal happy path:

```sh
cargo run --release -- serve
```

That one command builds the release binary if needed, then starts the local service. By default, the service reads `~/.copilot-tokens.json`, so you do **not** need a separate auth-export step if your existing Claude Code Router / Copilot auth flow already keeps a fresh token there.

If the token is missing or expired, refresh/re-login with your existing Copilot auth flow, then rerun the same command.

After a release build exists, you can also run the binary directly:

```sh
./target/release/codex-code-router serve
```

No subcommand also starts the service:

```sh
./target/release/codex-code-router
```

## Auth

The preferred local workflow is service-owned upstream auth: Codex points at the local endpoint, and the service obtains the upstream Copilot bearer token from one of these sources:

1. `COPILOT_BEARER_TOKEN`
2. `COPILOT_TOKEN_FILE`, defaulting to `~/.copilot-tokens.json`, with a `copilotToken` field
3. An incoming `Authorization` header from Codex provider command auth, if neither service-owned source is available

For the usual local setup, source 2 is enough: keep `~/.copilot-tokens.json` fresh and run `cargo run --release -- serve`.

The token-file shape is compatible with the existing CCR-style file:

```json
{
  "githubToken": "...",
  "copilotToken": "...",
  "endpoint": "...",
  "expiresAt": 1790000000,
  "lastUpdated": 1789990000
}
```

If `expiresAt` is present and expired or near expiry, the service refuses to use the token. This Rust implementation currently **does not implement GitHub Copilot OAuth/device refresh**. Refresh with an existing trusted Copilot auth flow, or provide a fresh token through `COPILOT_BEARER_TOKEN` or `COPILOT_TOKEN_FILE`.

### Token helper subcommand

If you prefer Codex command-backed auth, the Rust binary can print the configured token:

```sh
./target/release/codex-code-router print-token
```

For this subcommand, stdout contains only the bearer token. Diagnostics go to stderr.

## Codex config

Add a dedicated Copilot-backed profile in your global Codex config, usually `~/.codex/config.toml`. Do not replace your personal/default Codex account profile; keep it separate and switch profiles when you want Copilot-backed execution.

### Service-owned auth profile

Use this when the service has `COPILOT_BEARER_TOKEN` or, more commonly, a readable `~/.copilot-tokens.json` token file:

```toml
[model_providers.copilot-proxy]
name = "GitHub Copilot Proxy"
base_url = "http://127.0.0.1:60001/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 4
stream_max_retries = 5
stream_idle_timeout_ms = 300000

[profiles.copilot]
model_provider = "copilot-proxy"
model = "gpt-5.5"
```

### Optional command-backed auth profile

Use this only if you want Codex to attach the Copilot token to local requests instead of relying on service-owned auth:

```toml
[model_providers.copilot-proxy]
name = "GitHub Copilot Proxy"
base_url = "http://127.0.0.1:60001/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 4
stream_max_retries = 5
stream_idle_timeout_ms = 300000

[model_providers.copilot-proxy.auth]
command = "/Users/dpearson/repos/codex-code-router/target/release/codex-code-router"
args = ["print-token"]
refresh_interval_ms = 240000

[profiles.copilot]
model_provider = "copilot-proxy"
model = "gpt-5.5"
```

Keep your normal/default profile separate, for example:

```toml
[profiles.default]
model_provider = "openai"
model = "gpt-5"
```

Then switch with Codex profiles, for example `codex --profile copilot`. The service is not a wrapper that launches Codex.

The recurring workflow is therefore just:

```sh
cd /Users/dpearson/repos/codex-code-router && cargo run --release -- serve
```

Then use Codex separately with the `copilot` profile when you want the Copilot-backed provider.

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `HOST` | `127.0.0.1` | Local bind host. |
| `PORT` | `60001` | Local bind port. |
| `COPILOT_RESPONSES_URL` | `https://api.githubcopilot.com/responses` | Upstream Responses endpoint. |
| `COPILOT_MODELS_URL` | `https://api.githubcopilot.com/models` | Upstream models endpoint. |
| `COPILOT_BEARER_TOKEN` | unset | Service-owned upstream Copilot token. |
| `COPILOT_TOKEN_FILE` | `~/.copilot-tokens.json` | Service-owned token file with `copilotToken`. |
| `COPILOT_TOKEN_EXPIRY_BUFFER_SECONDS` | `300` | Refuse token-file tokens near expiry. |
| `REQUEST_TIMEOUT_MS` | `300000` | Upstream request timeout. |
| `COPILOT_CHAT_VERSION` | `0.35.0` | Copilot Chat header version. |
| `COPILOT_EDITOR_VERSION` | `vscode/1.109.2` | Editor header version. |
| `GITHUB_API_VERSION` | `2025-10-01` | GitHub API version header. |
| `RATE_LIMIT_MAX_TOTAL_WAIT_MS` | `0` | `0` means unlimited total wait for HTTP `429`. |
| `RATE_LIMIT_MAX_SLEEP_MS` | `60000` | Maximum sleep for one rate-limit retry. |
| `RATE_LIMIT_INITIAL_BACKOFF_MS` | `1000` | Fallback initial delay when no usable rate-limit headers are present. |
| `RATE_LIMIT_BACKOFF_MULTIPLIER` | `2` | Fallback exponential multiplier. |

See `.env.example` for a copyable local template.

## Development and validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The test suite covers header injection/redaction, token loading, `/health`, `/v1/models` proxying, `/v1/responses` SSE passthrough, HTTP `429` retry behavior, auth failure behavior, and unsupported routes.

## LiteLLM status

LiteLLM was investigated earlier as a possible replacement. It is no longer the recommended or current implementation path for this repo because the project decision is to keep a minimal custom Rust byte-forwarding adapter. The historical note remains in `docs/litellm-copilot-responses-plan.md`; the runnable LiteLLM config was removed.
