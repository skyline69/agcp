# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

AGCP (Anthropic → Google Cloud Code Proxy) is a lightweight Rust proxy that translates between Anthropic's Claude API and Google's Cloud Code API. It enables using Claude models and Gemini models through an Anthropic-compatible interface. This is a Rust port of the Node.js [antigravity-claude-proxy](./antigravity-claude-proxy/).

## Commands

```bash
# Build (debug)
cargo build

# Build (optimized release with LTO)
cargo build --release

# Run (default: port 8080, host 127.0.0.1)
cargo run --release

# Run with options
cargo run --release -- --port 3000 --host 0.0.0.0 --debug

# First-time setup: OAuth login
cargo run --release -- --login

# Run tests
cargo test

# Run a single test
cargo test test_model_family
cargo test test_is_thinking
```

## Architecture

**Request Flow:**
```
Claude Code CLI → HTTP Server (hyper) → CloudCodeClient → Google Cloud Code API
```

**Module Structure:**

```
src/
├── main.rs           # Entry point, CLI parsing, OAuth login flow, server bootstrap
├── server.rs         # HTTP server (hyper), request routing, Anthropic API endpoints
├── config.rs         # Configuration from ~/.config/agcp/config.toml
├── error.rs          # Error types (Auth, Api, Http, Timeout)
├── models.rs         # Model definitions and family detection
│
├── auth/             # Authentication
│   ├── mod.rs        # HttpClient for OAuth operations
│   ├── oauth.rs      # OAuth PKCE flow, callback server
│   └── token.rs      # Account persistence, token refresh
│
├── cloudcode/        # Cloud Code API client
│   ├── mod.rs        # Re-exports
│   ├── client.rs     # HTTPS client with retry/failover logic
│   ├── discover.rs   # Project discovery via loadCodeAssist API
│   ├── rate_limit.rs # Rate limit parsing, backoff constants, deduplication
│   ├── request.rs    # Request building and header construction
│   ├── response.rs   # Response parsing (non-streaming)
│   └── sse.rs        # SSE parser for streaming responses
│
└── format/           # API format conversion
    ├── mod.rs        # Re-exports
    ├── anthropic.rs  # Anthropic Messages API types
    ├── google.rs     # Google GenerativeAI types (Cloud Code wrapper)
    ├── to_anthropic.rs  # Google → Anthropic response conversion
    └── to_google.rs     # Anthropic → Google request conversion
```

**Key Types:**

- `ServerState`: Shared state containing account, HTTP client, and Cloud Code client
- `Account`: OAuth credentials and project ID, persisted to `~/.config/agcp/account.json`
- `CloudCodeClient`: HTTPS client with dual-endpoint fallback (daily/prod)
- `MessagesRequest`/`MessagesResponse`: Anthropic API types
- `CloudCodeRequest`/`GenerateContentResponse`: Google API types

**Configuration:**

- Config file: `~/.config/agcp/config.toml`
- Account file: `~/.config/agcp/accounts.json`
- Default port: 8080, host: 127.0.0.1

## API Endpoints

- `POST /v1/messages` - Anthropic Messages API (streaming and non-streaming)
- `GET /v1/models` - List available models
- `GET /health`, `GET /` - Health check

## Model Mapping

Models are defined in `src/models.rs`. The proxy supports:

- Claude models: `claude-opus-4-6-thinking`, `claude-opus-4-5-thinking`, `claude-sonnet-4-5`, `claude-sonnet-4-5-thinking`
- Gemini 2.5 models: `gemini-2.5-flash`, `gemini-2.5-flash-lite`, `gemini-2.5-flash-thinking`, `gemini-2.5-pro`
- Gemini 3 models: `gemini-3-flash`, `gemini-3-pro-high`, `gemini-3-pro-image`, `gemini-3-pro-low`

**Thinking Model Detection:**
- Claude: requires "thinking" in model name
- Gemini: "thinking" in name OR version 3+ (e.g., `gemini-3-flash` is a thinking model)

## Cloud Code Client

Located in `src/cloudcode/client.rs`:

- Dual endpoint fallback: tries `daily-cloudcode-pa.googleapis.com` first, then `cloudcode-pa.googleapis.com`
- Automatic retry with exponential backoff for 429 rate limits (up to 5 retries)
- HTTP/2 support for streaming
- Request timeout: 120 seconds
- Concurrency limiting: max 1 concurrent request with 500ms minimum interval
- Cached HTTP client (reused across requests)

## Format Conversion

The proxy translates between API formats:

1. **Anthropic → Google** (`src/format/to_google.rs`):
   - `MessagesRequest` → `CloudCodeRequest`
   - Content blocks → Parts (text, images, tool calls, tool results)
   - System prompt → `systemInstruction`
   - Tools → `functionDeclarations`

2. **Google → Anthropic** (`src/format/to_anthropic.rs`):
   - `GenerateContentResponse` → `MessagesResponse`
   - Parts → Content blocks
   - `finishReason` → `stop_reason`
   - Usage metadata → token counts

## Streaming (SSE)

The `SseParser` in `src/cloudcode/sse.rs`:
- Parses Google SSE format (`data:` prefixed JSON)
- Emits Anthropic stream events (`message_start`, `content_block_*`, `message_delta`, `message_stop`)
- Handles thinking blocks for thinking-enabled models
- Accumulates tool use JSON across delta events

## Testing

The `test_api.sh` script tests direct API requests to Cloud Code endpoints for debugging.

Tests in `src/models.rs`:
- `test_model_family`: Model family detection
- `test_is_thinking`: Thinking model identification
