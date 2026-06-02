# codex-code-router

`codex-code-router` is a small local service that lets Codex CLI use GitHub Copilot as a Responses API provider.

It runs on your machine, listens on `127.0.0.1:60001` by default, handles GitHub Copilot authentication, and forwards Codex Responses API traffic to GitHub Copilot with the provider headers Copilot expects.

## Table of contents

- [Quick start](#quick-start)
- [What you get](#what-you-get)
- [Requirements](#requirements)
- [Install from a local checkout](#install-from-a-local-checkout)
- [Update installed binaries](#update-installed-binaries)
- [Sign in to GitHub Copilot](#sign-in-to-github-copilot)
- [Start the local service](#start-the-local-service)
- [Configure Codex](#configure-codex)
- [Optional: command-backed auth](#optional-command-backed-auth)
- [Configuration reference](#configuration-reference)
- [Logs and diagnostics](#logs-and-diagnostics)
- [Troubleshooting](#troubleshooting)
- [Development](#development)

## Quick start

Install from crates.io, then sign in and start the local service:

```sh
cargo install codex-code-router
ccrx login
ccrx start
```

Add this provider block to `~/.codex/config.toml`:

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
```

Then select the `copilot-proxy` provider in Codex and choose whichever Copilot model you want to use. Future updates can be run from any folder with `ccrx update`.

## What you get

- A local OpenAI-compatible Responses endpoint for Codex: `http://127.0.0.1:60001/v1`.
- GitHub Copilot device login from the command line.
- Automatic refresh for saved Copilot tokens.
- Background service commands for starting, stopping, restarting, and checking status.
- Safe metadata logs for troubleshooting, without printing bearer tokens or request bodies.
- HTTP `429` rate-limit retries based on provider status and headers.

The adapter is intentionally narrow: it supports Codex-style Responses API traffic only. It does not provide a Chat Completions endpoint, Docker workflow, or broad multi-provider routing.

## Why use this instead of LiteLLM?

LiteLLM is useful when you want a general provider gateway, but that flexibility usually comes with protocol normalization and request/response transformation. Codex already speaks the Responses API shape this project needs, so `codex-code-router` keeps the path thinner: it handles Copilot auth, headers, endpoint mapping, token refresh, and HTTP `429` retries while leaving Codex request bodies and Responses SSE streams as unchanged as possible.

## Requirements

- A GitHub account with access to GitHub Copilot.
- Codex CLI installed and working locally.
- Rust and Cargo installed. Cargo builds the binaries locally from crates.io.

## Install from a local checkout

For development, clone the repository and install both binaries from your local checkout:

```sh
git clone https://github.com/dpearson2699/codex-code-router.git codex-code-router
cd codex-code-router
cargo install --path . --bins --locked
```

This installs:

- `ccrx` — the short service-control command you will normally use.
- `codex-code-router` — the full binary, useful for foreground runs and command-backed auth.

The source checkout can live anywhere, such as `~/repos/codex-code-router`, and is not used by the background service after installation. Cargo installs the runnable binaries into its install `bin` directory, usually `~/.cargo/bin` unless your environment uses a different Cargo install root. The `~/.codex-code-router` directory is only runtime state for the background service, such as the PID file and logs; it is not intended to be the binary install location.

If you do not want to install the binaries globally, you can run from the checkout instead:

```sh
cargo run --release -- serve
```

## Update installed binaries

After you have a version with the update command installed, the normal update flow is:

```sh
ccrx update
```

You can run that from any directory. It does not require a local checkout. By default, `ccrx update` reinstalls the latest `codex-code-router` package from crates.io and restarts the background service. When possible, it infers the existing Cargo install root from the running `ccrx` binary, so the updated binaries land in the same install location.

To test an unreleased build from the latest GitHub `main` branch:

```sh
ccrx update --branch main
```

To install from a specific GitHub tag:

```sh
ccrx update --tag v0.1.0
```

If you want a repeatable first install pinned to a specific crates.io version, use Cargo's package-version form:

```sh
cargo install codex-code-router@1.0.0
```

To install without restarting immediately:

```sh
ccrx update --no-restart
```

If your installed `ccrx` is too old to have the `update` command, bootstrap once from your cloned checkout:

```sh
cd /path/to/codex-code-router
git pull --ff-only
cargo install --path . --bins --locked --force
ccrx restart
ccrx status
```

That keeps your existing Codex config and saved Copilot token file. It only updates the installed `ccrx` and `codex-code-router` binaries, then restarts the background service so Codex uses the new code. Future updates can use `ccrx update` from any folder.

If the service was not already running, use `ccrx start` instead of `ccrx restart`.

If `git pull --ff-only` reports local changes in your checkout, stop and review them before updating. Either commit them, stash them, or use `ccrx update` so Cargo installs from crates.io without touching your local files.

## Sign in to GitHub Copilot

Run the built-in device login:

```sh
ccrx login
```

The command prints a GitHub verification URL and one-time code. After you approve the device login in your browser, the service saves a local token file at:

```text
~/.copilot-tokens.json
```

Token values are not printed. On Unix-like systems, the saved token file is written with `0600` permissions.

You can also skip `ccrx login` and let `ccrx start` prompt you the first time authentication is needed.

## Start the local service

Start the adapter in the background:

```sh
ccrx start
```

Useful service commands:

```sh
ccrx status
ccrx update
ccrx restart
ccrx stop
```

By default, the service uses:

- Local base URL: `http://127.0.0.1:60001/v1`
- Health check: `http://127.0.0.1:60001/health`
- PID file: `~/.codex-code-router/codex-code-router.pid`
- Normal log file: `~/.codex-code-router/codex-code-router.log`

To run in the foreground instead:

```sh
codex-code-router serve
```

No subcommand also starts the foreground service:

```sh
codex-code-router
```

## Configure Codex

Add a Copilot-backed provider to your Codex config, usually `~/.codex/config.toml`:

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
```

That provider block is the required Codex-side setup. After it exists, you can switch to `copilot-proxy` in Codex's model/provider controls and choose the model there.

If you want this provider to be your default in Codex, set the top-level defaults in the same `~/.codex/config.toml`:

```toml
model_provider = "copilot-proxy"
# Optional: set this only if you want to pin a default model.
model = "<copilot-model>"
```

If you prefer a named CLI profile, add it to `~/.codex/config.toml`:

```toml
[profiles.copilot]
model_provider = "copilot-proxy"
# Optional: set this only if this profile should pin a model.
model = "<copilot-model>"
```

Then run Codex with that profile when desired:

```sh
codex --profile copilot
```

You do not need a separate `copilot.config.toml` file for the local provider to work. A separate profile file may be useful for some personal workflows, but it is optional and can pin a model if you put a `model = ...` value in it.

For day-to-day use, start the adapter and use Codex normally:

```sh
ccrx start
```

## Optional: command-backed auth

The recommended setup is service-owned auth: `ccrx start` reads and refreshes `~/.copilot-tokens.json`, and Codex only talks to the local endpoint.

If you prefer Codex to attach the Copilot bearer token to local requests, configure command-backed auth with an absolute path to the installed binary:

```toml
[model_providers.copilot-proxy.auth]
command = "/absolute/path/to/codex-code-router"
args = ["print-token"]
refresh_interval_ms = 240000
```

For `print-token`, stdout contains only the bearer token. Diagnostics go to stderr.

## Authentication details

The service chooses upstream Copilot auth in this order:

1. `COPILOT_BEARER_TOKEN`
2. `COPILOT_TOKEN_FILE`, defaulting to `~/.copilot-tokens.json`
3. An incoming `Authorization` header from Codex, if neither service-owned source is available

The token file contains a GitHub token plus a Copilot token. When the Copilot token is expired or near expiry, the service refreshes it from the saved GitHub token and rewrites the same file without printing secrets.

If GitHub Copilot returns upstream HTTP `401 Unauthorized` for an otherwise locally-valid token-file token, the service force-refreshes the token file once and replays the original request body bytes. That reactive refresh is only used for token-file auth.

## Configuration reference

Most users only need the Codex provider block and `ccrx login`. The settings below are available when you need to customize ports, auth, headers, retries, or diagnostics.

### Codex provider settings

| Setting | Recommended value | Purpose |
| --- | --- | --- |
| `base_url` | `http://127.0.0.1:60001/v1` | Points Codex at the local adapter. |
| `wire_api` | `"responses"` | Uses Codex's native Responses API wire format. |
| `requires_openai_auth` | `false` | Lets the local service own upstream Copilot auth. Set to `true` only if a Codex app requires an API-key-shaped placeholder. |
| `supports_websockets` | `false` | Keeps Codex on HTTP Responses streaming. |
| `request_max_retries` | `4` | Codex-side request retry count. |
| `stream_max_retries` | `5` | Codex-side stream retry count. |
| `stream_idle_timeout_ms` | `300000` | Codex-side stream idle timeout. |
| top-level or profile `model_provider` | `"copilot-proxy"` | Selects this provider as a default or named-profile provider. |
| top-level or profile `model` | optional | Pins a default model only if you want one. Omit it when you prefer to choose models through Codex's normal UI/CLI controls. |

### Service environment variables

| Variable | Default | Purpose |
| --- | --- | --- |
| `HOST` | `127.0.0.1` | Local bind host. |
| `PORT` | `60001` | Local bind port. |
| `COPILOT_RESPONSES_URL` | `https://api.githubcopilot.com/responses` | Upstream Responses endpoint. |
| `COPILOT_MODELS_URL` | `https://api.githubcopilot.com/models` | Upstream models endpoint. |
| `COPILOT_BEARER_TOKEN` | unset | Service-owned Copilot bearer token. If set, it takes precedence over token-file auth. |
| `COPILOT_TOKEN_FILE` | `~/.copilot-tokens.json` | Token file used by service-owned auth. |
| `COPILOT_TOKEN_EXPIRY_BUFFER_SECONDS` | `300` | Refresh token-file auth before it is too close to expiry. |
| `COPILOT_TOKEN_REFRESH` | `true` | Refresh expired token-file values and enable one reactive HTTP `401` refresh. |
| `COPILOT_TOKEN_URL` | `https://api.github.com/copilot_internal/v2/token` | GitHub endpoint used to exchange a GitHub token for a Copilot token. |
| `GITHUB_DEVICE_CODE_URL` | `https://github.com/login/device/code` | GitHub device-code endpoint used by `ccrx login`. |
| `GITHUB_ACCESS_TOKEN_URL` | `https://github.com/login/oauth/access_token` | GitHub device-token polling endpoint used by `ccrx login`. |
| `GITHUB_OAUTH_CLIENT_ID` | `01ab8ac9400c4e429b23` | OAuth client ID used for device login. |
| `GITHUB_OAUTH_SCOPE` | `read:user` | OAuth scope requested during device login. |
| `REQUEST_TIMEOUT_MS` | `300000` | Upstream request timeout. |
| `REQUEST_BODY_LIMIT_BYTES` | `0` | Maximum local inbound request body size before forwarding; `0` disables the local limit. |
| `COPILOT_CHAT_VERSION` | `0.35.0` | Copilot Chat version header sent upstream. |
| `COPILOT_EDITOR_VERSION` | `vscode/1.109.2` | Editor version header sent upstream. |
| `GITHUB_API_VERSION` | `2025-10-01` | GitHub API version header sent upstream. |
| `RATE_LIMIT_MAX_TOTAL_WAIT_MS` | `0` | Total HTTP `429` retry budget; `0` means unlimited wait. |
| `RATE_LIMIT_MAX_SLEEP_MS` | `60000` | Maximum single sleep for one HTTP `429` retry. |
| `RATE_LIMIT_INITIAL_BACKOFF_MS` | `1000` | Fallback initial backoff when no usable rate-limit header is present. |
| `RATE_LIMIT_BACKOFF_MULTIPLIER` | `2` | Fallback exponential backoff multiplier. |
| `RUST_LOG` | unset | Optional tracing filter for normal logs. |
| `CODEX_CODE_ROUTER_RAW_LOG_LEVEL` | `off` | Raw diagnostics mode: `off`, `metadata`, `content_redacted`, or `full_content`. |
| `CODEX_CODE_ROUTER_RAW_LOG_FILE` | `~/.codex-code-router/raw/diagnostics.jsonl` | Raw diagnostic JSONL output path. |
| `CODEX_CODE_ROUTER_RAW_LOG_MAX_BYTES` | `65536` | Maximum size for one raw diagnostic event. |
| `CODEX_CODE_ROUTER_RAW_LOG_CONTENT_MAX_BYTES` | `16384` | Maximum captured request/response content bytes per raw diagnostic event. |

Boolean environment variables accept `1`, `true`, `yes`, or `on` for true and `0`, `false`, `no`, or `off` for false. See `.env.example` for a copyable local template.

## Logs and diagnostics

Normal logs are metadata-only. They include service startup, request lifecycle, auth source summaries, upstream status, retry decisions, and stream byte/chunk counts.

Normal logs do **not** include request/response bodies, bearer tokens, GitHub OAuth tokens, authorization header values, cookies, token-file contents, or encrypted reasoning content.

Foreground `serve` writes normal logs to stderr. Background service logs are written to:

```text
~/.codex-code-router/codex-code-router.log
```

When `RUST_LOG` is unset, the service uses this normal-log filter:

```text
codex_code_router=info,warn
```

Set `RUST_LOG` when you need more or less detail. For example:

```sh
RUST_LOG=codex_code_router=debug,warn ccrx restart
```

Check service state and log location with:

```sh
ccrx status
```

Raw diagnostic JSONL is off by default and should be treated as sensitive when enabled:

```sh
CODEX_CODE_ROUTER_RAW_LOG_LEVEL=metadata ccrx restart
```

The default raw diagnostic path is:

```text
~/.codex-code-router/raw/diagnostics.jsonl
```

Use raw diagnostics only while reproducing a problem, then turn them off again.

Raw diagnostic levels:

| Level | Behavior |
| --- | --- |
| `off` | Writes no raw diagnostic events. |
| `metadata` | Writes request/retry/stream metadata only, without body snapshots. |
| `content_redacted` | Writes bounded content snapshots with string values redacted. |
| `full_content` | Writes bounded content snapshots while still redacting known token-like fields. Treat this mode as sensitive. |

## Troubleshooting

### Check whether the service is running

```sh
ccrx status
```

Or query the health endpoint:

```sh
curl http://127.0.0.1:60001/health
```

### Re-run login

If auth fails or the saved GitHub token has been revoked:

```sh
ccrx login
ccrx restart
```

### Codex asks for an API key

Some Codex app builds require custom providers to declare OpenAI auth before model settings can be edited. If that happens, set `requires_openai_auth = true` for the local provider and enter a placeholder API-key-shaped value when the app asks.

The service still prefers its own Copilot token from `ccrx login` / `~/.copilot-tokens.json`, so the placeholder is not forwarded upstream when service-owned auth is available.

### Requests appear to hang

Check the log path printed by `ccrx status`. If the latest entries show an upstream HTTP `429`, the adapter is waiting for GitHub Copilot's rate-limit window. By default, `RATE_LIMIT_MAX_TOTAL_WAIT_MS=0`, which allows an unlimited total wait for HTTP `429` responses.

### Local `/v1/responses` returns `Payload Too Large`

If Codex reports `413 Payload Too Large` with `Failed to buffer the request body: length limit exceeded` from `http://127.0.0.1:60001/v1/responses`, the local adapter rejected the request before it reached GitHub Copilot. By default, the local adapter does not impose a request body limit. If you configured `REQUEST_BODY_LIMIT_BYTES` to a finite value, set it higher or set it back to `0`, then restart the service.

## Development

Run the standard checks before opening a pull request:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The test suite covers header injection, redaction, token loading and refresh, device login, `/health`, `/v1/models`, `/v1/responses`, HTTP `429` retry behavior, reactive HTTP `401` token refresh, diagnostics, unsupported routes, and the strict `print-token` stdout contract.
