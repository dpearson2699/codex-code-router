# Agent instructions for `codex-code-router`

This repo is a small, deliberately thin Rust adapter for using Codex CLI with GitHub Copilot's Responses API endpoint.

The repository is now the **custom Rust Codex CLI → GitHub Copilot Responses API adapter**. LiteLLM is not the current implementation path, and Node/TypeScript is no longer the runtime path.

## Read first

Before changing code, read:

1. `AGENTS.md` — core doctrine and safety rules.
2. `docs/fresh-session-brief.md` — current repo state and first-session checklist.
3. `docs/rust-implementation-plan.md` — current Rust implementation notes and future constraints.
4. `docs/codex-cli-copilot-port-feasibility.md` — feasibility analysis transferred from `.claude-code-router`.
5. `docs/rate-limit-retry-policy.md` — status/header-based retry behavior for upstream HTTP 429s.

`docs/litellm-copilot-responses-plan.md` is historical only. Do not treat LiteLLM as the recommended path unless a future explicit product decision reverses this one.

## Core doctrine

- **Provider adapter, not transformer.** Treat GitHub Copilot like an OpenAI-compatible cloud provider with different auth, headers, and endpoint conventions.
- **Responses-only.** Do not add Chat Completions, Anthropic Messages, or Anthropic SSE conversion paths.
- **Codex-native request shape.** Let Codex own request bodies, tool schemas, reasoning settings, history, compaction behavior, and SSE parsing.
- **Stream bytes through.** The proxy should forward request bodies and response streams with as little interpretation as possible.
- **No proactive tool rewriting or truncation.** Only add targeted compatibility fixes after a concrete Copilot error proves they are required.
- **Rate-limit retry is allowed.** HTTP `429` retry based on status/headers is a provider transport behavior, not protocol transformation.
- **Every mutation must be explicit.** If request or response mutation becomes necessary, keep it isolated, documented, logged, and tested.

## Hard constraints

- Do **not** recreate CCR's transformer architecture.
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
- If service-owned auth is unavailable, the adapter can forward an incoming Codex `Authorization` header.
- The adapter retries upstream HTTP `429` rate limits before sending downstream bytes, following `docs/rate-limit-retry-policy.md`.
- The adapter passes Responses SSE back unchanged.

## Implementation direction

- Rust is canonical.
- Preserve the external contract: `GET /health`, `GET /v1/models`, `POST /v1/responses`.
- Preserve the minimal-adapter doctrine; do not port TypeScript or CCR behavior for its own sake.
- If a body compatibility pass becomes necessary, isolate it behind tests and document exactly why.

## Fresh-session onboarding

- Read `AGENTS.md` and `docs/fresh-session-brief.md` at the start of a new assistant session.
- Run the Rust validation commands before and after behavioral changes when possible.
- If the user asks about LiteLLM, state that it is a historical investigation and not the current path unless they explicitly ask to revisit that decision.

## Commands

- Run in development: `cargo run -- serve`
- Print token helper output: `cargo run --quiet -- print-token`
- Format check: `cargo fmt --check`
- Test: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Build release binary: `cargo build --release`

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
