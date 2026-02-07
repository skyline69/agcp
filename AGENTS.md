# AGENTS.md

Guidance for AI coding agents working with this Rust codebase.

## Project Overview

AGCP (Anthropic → Google Cloud Code Proxy) is a Rust proxy that translates between Anthropic's Claude API and Google's Cloud Code API. It enables Claude and Gemini models through an Anthropic-compatible interface.

## Commands

```bash
# Build
cargo build                 # Debug build
cargo build --release       # Optimized with LTO

# Run
cargo run --release
cargo run --release -- --port 3000 --host 0.0.0.0 --debug

# Test
cargo test                  # Run all tests
cargo test test_model_family           # Single test by name
cargo test models::tests::test_is_thinking  # Fully qualified
cargo test -- --nocapture   # Show println! output

# Linting and formatting
cargo fmt                   # Format code
cargo clippy -- -D warnings # Lint (treat warnings as errors)

# Subcommands
agcp login                  # OAuth authentication
agcp status                 # Show proxy status
agcp doctor                 # Diagnose configuration
```

## Architecture

```
src/
├── main.rs           # Entry point, CLI, daemon mode
├── server.rs         # HTTP server (hyper), API routing
├── config.rs         # TOML config, global state
├── error.rs          # Error types (thiserror)
├── models.rs         # Model definitions, aliases
├── cache.rs          # LRU response cache
├── auth/             # OAuth, accounts, tokens
├── cloudcode/        # Google Cloud Code client
│   ├── client.rs     # HTTPS with retry/failover
│   ├── rate_limit.rs # Backoff, deduplication
│   └── sse.rs        # SSE streaming parser
└── format/           # API conversion
    ├── anthropic.rs  # Anthropic types
    ├── google.rs     # Google types
    ├── to_google.rs  # Anthropic → Google
    └── to_anthropic.rs  # Google → Anthropic
```

## Code Style

### Imports

Order: std → external crates → internal modules, with blank lines between groups:

```rust
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
```

### Naming Conventions

| Type | Convention | Example |
|------|-----------|---------|
| Types/Structs | PascalCase | `CloudCodeClient`, `MessagesRequest` |
| Functions | snake_case | `get_model_family`, `is_thinking_model` |
| Constants | SCREAMING_SNAKE_CASE | `MAX_RETRIES`, `API_TIMEOUT` |
| Modules | snake_case | `rate_limit`, `to_anthropic` |

### Constants

Define at module top, group by purpose:

```rust
const API_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RETRIES: u32 = 5;
```

### Error Handling

Use `thiserror` for error types. Define domain-specific error enums:

```rust
#[derive(Debug, Error)]
pub enum Error {
    #[error("authentication error: {0}")]
    Auth(#[from] AuthError),

    #[error("api error: {0}")]
    Api(#[from] ApiError),
}

pub type Result<T> = std::result::Result<T, Error>;
```

Add `suggestion()` methods for user-friendly recovery hints.

### Serde Patterns

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]  // or "kebab-case"
pub struct Config {
    #[serde(default)]
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<String>,
}

#[serde(untagged)]       // For variant enums without type field
#[serde(tag = "type")]   // For tagged enums
```

### Async Patterns

- Use `tokio` runtime with `#[tokio::main]`
- `Arc<RwLock<T>>` for shared mutable state
- `tokio::sync::Mutex` for async-safe locks

### Global State

Use `LazyLock` for lazy static initialization:

```rust
static GLOBAL_CONFIG: LazyLock<RwLock<Config>> =
    LazyLock::new(|| RwLock::new(Config::load().unwrap_or_default()));
```

## Testing

Tests are inline in source files:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_function_name() {
        let input = create_test_input();
        let result = function_under_test(input);
        assert_eq!(result, expected);
    }
}
```

Naming: `test_` prefix + what's being tested (e.g., `test_model_family`).

## Key Implementation Details

### Thinking Models

- Claude: requires "thinking" in model name
- Gemini 3+: all are thinking models regardless of name
- Must use streaming endpoint even for non-streaming requests

### Retry Logic

- Dual endpoint fallback (daily-cloudcode → cloudcode)
- Exponential backoff for 429s (up to 5 retries)
- Capacity exhaustion with tiered backoff

### Streaming (SSE)

- `SseParser` handles Google SSE format (`data:` prefixed JSON)
- Emits Anthropic-compatible stream events
- Handles thinking blocks with signature caching

## Files to Know

- `src/models.rs` - Model definitions, aliases, thinking detection
- `src/error.rs` - Error types and suggestions
- `src/cloudcode/client.rs` - Core API client with retry logic
- `src/format/to_google.rs` - Anthropic → Google conversion
- `src/config.rs` - Configuration loading and global state
