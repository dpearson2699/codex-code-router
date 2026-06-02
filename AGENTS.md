# Agent instructions for `codex-code-router`

This repo is a small, deliberately thin Rust adapter for using Codex CLI with GitHub Copilot's Responses API endpoint.

## Read first

Before changing code, read the public project guidance:

1. `AGENTS.md` — core doctrine and safety rules.
2. `README.md` — usage, configuration, validation, and public-facing behavior.

## Core doctrine

- **Provider adapter, not transformer.** Treat GitHub Copilot like an OpenAI-compatible cloud provider with different auth, headers, and endpoint conventions.
- **Responses-only.** Do not add Chat Completions, Anthropic Messages, or Anthropic SSE conversion paths.
- **Codex-native request shape.** Let Codex own request bodies, tool schemas, reasoning settings, history, compaction behavior, and SSE parsing.
- **Stream bytes through.** The proxy should forward request bodies and response streams with as little interpretation as possible.
- **No proactive tool rewriting or truncation.** Only add targeted compatibility fixes after a concrete Copilot error proves they are required.
- **Rate-limit retry is allowed.** HTTP `429` retry based on status/headers is a provider transport behavior, not protocol transformation.
- **Every mutation must be explicit.** If request or response mutation becomes necessary, keep it isolated, documented, logged, and tested.

## Hard constraints

- Do **not** recreate a transformer architecture.
- Do **not** add Chat Completions, Anthropic Messages, or Anthropic SSE bridges.
- Do **not** rewrite, truncate, summarize, or reshape tool calls by default.
- Do **not** parse request bodies unless a specific, tested Copilot compatibility fix requires it.
- Do **not** parse response-message text to detect rate limits; use HTTP `429` and headers only.
- Do **not** log bearer tokens, GitHub OAuth tokens, authorization headers, or unredacted request dumps.
- Do **not** add Docker/container support.
- Keep the model believing it is running inside **Codex**. GitHub Copilot is only the upstream provider.

## Current intended shape

- Codex points at this adapter as a custom provider with `wire_api = "responses"`.
- Codex uses `base_url = "http://127.0.0.1:60001/v1"` and `supports_websockets = false`.
- The adapter maps local `GET /v1/models` to Copilot's upstream `/models` endpoint.
- The adapter maps local `POST /v1/responses` to Copilot's upstream `/responses` endpoint.
- The adapter injects Copilot/VS Code-style provider headers.
- The adapter can own upstream Copilot auth from `COPILOT_BEARER_TOKEN` or `~/.copilot-tokens.json`.
- The adapter can refresh expired Copilot token-file values from a saved `githubToken` and can create that token file through interactive GitHub OAuth device login.
- For refreshable token-file auth, the adapter can force-refresh once after upstream HTTP `401` and replay the original request bytes.
- If service-owned auth is unavailable, the adapter can forward an incoming Codex `Authorization` header.
- The adapter retries upstream HTTP `429` rate limits before sending downstream bytes, based on HTTP status and rate-limit headers only.
- The adapter passes Responses SSE back unchanged.

## Implementation direction

- Rust is canonical.
- Preserve the external contract: `GET /health`, `GET /v1/models`, `POST /v1/responses`.
- Preserve the minimal-adapter doctrine; do not add generic protocol translation behavior for its own sake.
- If a body compatibility pass becomes necessary, isolate it behind tests and document exactly why.

## Fresh-session onboarding

- Read `AGENTS.md` and `README.md` at the start of a new assistant session.
- Run the Rust validation commands before and after behavioral changes when possible.

## Commands

- Run local service after install: `ccrx start`
- Restart local service after install: `ccrx restart`
- Stop local service after install: `ccrx stop`
- Check local service after install: `ccrx status`
- Update installed binaries from GitHub: `ccrx update`
- Run interactive GitHub Copilot device login: `ccrx login`
- Run local service without installing the short command: `cargo run --release -- serve`
- Print token helper output: `cargo run --quiet -- print-token`
- Format check: `cargo fmt --check`
- Test: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Build release binary: `cargo build --release`

## Release flow for `ccrx update`

- Do not publish this project to npm, crates.io, Homebrew, or another package registry unless the user explicitly asks.
- The default `ccrx update` path discovers the latest GitHub Release for `dpearson2699/codex-code-router`, then runs `cargo install --git ... --tag <tag> --bins --locked --force` locally.
- `ccrx update` is intended to run from any current working directory; it should not depend on a local checkout.
- When possible, `ccrx update` infers the existing Cargo install root from the running binary path (`.../bin/ccrx`) and passes `--root <root>` to `cargo install` so updated binaries land beside the existing install.
- GitHub Release artifacts are optional. A source tag plus GitHub Release is enough because users build locally with Cargo.
- Before creating a release, run the Rust validation commands:
  - `cargo fmt --check`
  - `cargo test`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --release`
- To publish a release that `ccrx update` can discover:
  - `git tag vX.Y.Z`
  - `git push origin vX.Y.Z`
  - `gh release create vX.Y.Z --generate-notes`
- If only a Git tag exists and no GitHub Release exists, users can still update with `ccrx update --tag vX.Y.Z`, but plain `ccrx update` requires a GitHub Release.
- For unreleased testing from the default branch, use `ccrx update --branch main`.

## Coding conventions

- Keep Rust boring and maintainable.
- Prefer small modules with explicit responsibilities.
- Prefer byte-forwarding request/response behavior over JSON parsing.
- Use `axum` for the local server and `reqwest` for upstream HTTP.
- Keep logs on stderr and never include token-bearing values.
- Keep docs updated when behavior changes.

## Safety notes

- Never log bearer tokens or full authorization headers.
- Never log GitHub OAuth tokens.
- Redact sensitive headers in diagnostics and future request dumps.
- `codex-code-router print-token` stdout must contain only the token for Codex command-backed auth; diagnostics go to stderr.

## Debug helper: verify reasoning effort in raw diagnostics

When raw diagnostics are enabled and a task needs proof that Codex effort reached the proxy request, use the tightened request snapshot schema:

- event kind: `inbound_request_content`
- path: `fields.snapshot.extracted.reasoning_effort`

Level semantics:

- `metadata`: no content snapshot event
- `content_redacted`: value is `<redacted-content>`
- `full_content`: concrete effort value is visible (for example `high`)

Correlate with `fields.local_id` across `inbound_request`, `upstream_response_ready`, and stream terminal events for a full request trace.
