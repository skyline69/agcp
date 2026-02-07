<p align="center">
  <img src="images/logo.svg" alt="AGCP" width="380">
</p>

<p align="center">
  <a href="https://crates.io/crates/agcp"><img src="https://img.shields.io/crates/v/agcp.svg" alt="Crates.io"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.93+-orange.svg" alt="Rust"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License"></a>
</p>

<p align="center"><b>Extremely Lightweight Antigravity-Claude-Proxy</b></p>

A blazing-fast Rust proxy that translates Anthropic's Claude API to Google's Cloud Code API. Use Claude and Gemini models through a single Anthropic-compatible endpoint.

<p align="center">
  <img src="images/demo-1.png" alt="AGCP Overview — real-time request monitoring, stats, and logs" width="800">
</p>

<details>
<summary><b>More screenshots</b></summary>
<br>
<p align="center">
  <img src="images/demo-2.png" alt="AGCP Quota — per-model usage with donut charts" width="800">
</p>
<p align="center">
  <img src="images/demo-3.png" alt="AGCP About page" width="800">
</p>
</details>

## Why AGCP?

- **Lightweight** - Single binary, minimal dependencies, ~5MB compiled
- **Fast** - Written in Rust with async I/O, handles concurrent requests efficiently
- **Simple** - Just `agcp login` and you're ready, no config files needed
- **Powerful** - Multi-account support, response caching, smart load balancing

## Features

- **Anthropic API Compatible** - Works with Claude Code, OpenCode, Cursor, Cline, and other Anthropic API clients
- **Multiple Models** - Access Claude (Opus, Sonnet) and Gemini (Flash, Pro) through a single endpoint
- **Multi-Account Support** - Rotate between multiple Google accounts with smart load balancing
- **Response Caching** - Cache non-streaming responses to reduce quota usage
- **Interactive TUI** - Beautiful terminal UI for monitoring and configuration
- **Background Daemon** - Runs quietly in the background

## Quick Start

```bash
# Install from source
git clone https://github.com/skyline69/agcp
cd agcp
cargo build --release

# Login with Google OAuth
./target/release/agcp login

# Start the proxy (runs as background daemon)
./target/release/agcp

# Configure your AI tool to use http://127.0.0.1:8080
```

## Installation

### Homebrew (macOS/Linux)

```bash
brew tap skyline69/agcp
brew install agcp
```

### APT (Debian/Ubuntu)

```bash
# Add the GPG key and repository
curl -fsSL https://dasguney.com/apt/public.key | sudo gpg --dearmor -o /usr/share/keyrings/agcp.gpg
echo "deb [signed-by=/usr/share/keyrings/agcp.gpg] https://dasguney.com/apt stable main" | sudo tee /etc/apt/sources.list.d/agcp.list

# Install
sudo apt update
sudo apt install agcp
```

### DNF (Fedora/RHEL)

```bash
# Add the repository
sudo tee /etc/yum.repos.d/agcp.repo << 'EOF'
[agcp]
name=AGCP
baseurl=https://dasguney.com/rpm/packages
enabled=1
gpgcheck=1
gpgkey=https://dasguney.com/rpm/public.key
EOF

# Install
sudo dnf install agcp
```

### Nix

```bash
# Run directly
nix run github:skyline69/agcp

# Or install into profile
nix profile install github:skyline69/agcp
```

### From Source

```bash
git clone https://github.com/skyline69/agcp
cd agcp
cargo build --release

# Optional: Install to PATH
cp target/release/agcp ~/.local/bin/
```

### Shell Completions

```bash
# Bash
eval "$(agcp completions bash)"

# Zsh
eval "$(agcp completions zsh)"

# Fish
agcp completions fish > ~/.config/fish/completions/agcp.fish
```

## Usage

### Commands

| Command | Description |
|---------|-------------|
| `agcp` | Start the proxy server (daemon mode) |
| `agcp login` | Authenticate with Google OAuth |
| `agcp setup` | Configure AI tools to use AGCP |
| `agcp tui` | Launch interactive terminal UI |
| `agcp status` | Check if server is running |
| `agcp stop` | Stop the background server |
| `agcp restart` | Restart the background server |
| `agcp logs` | View server logs (follows by default) |
| `agcp config` | Show current configuration |
| `agcp accounts` | Manage multiple accounts |
| `agcp doctor` | Check configuration and connectivity |
| `agcp quota` | Show model quota usage |
| `agcp stats` | Show request statistics |
| `agcp test` | Verify setup works end-to-end |

### CLI Options

```bash
agcp [OPTIONS]

Options:
  -p, --port <PORT>    Port to listen on (default: 8080)
  --host <HOST>        Host to bind to (default: 127.0.0.1)
  --network            Listen on all interfaces (LAN access)
  -f, --foreground     Run in foreground instead of daemon mode
  -d, --debug          Enable debug logging
  --fallback           Enable model fallback on quota exhaustion
  -h, --help           Show help
  -V, --version        Show version
```

## Interactive TUI

AGCP includes a terminal UI for monitoring and configuration (`agcp tui`):

Features:
- **Overview** - Real-time request rate, response times, account status
- **Logs** - Syntax-highlighted log viewer with scrolling
- **Accounts** - Manage and monitor account quota
- **Config** - Edit configuration interactively
- **Quota** - Visual quota usage with donut charts

## Model Aliases

For convenience, you can use these short aliases:

| Alias | Model |
|-------|-------|
| `opus` | claude-opus-4-6-thinking |
| `sonnet` | claude-sonnet-4-5 |
| `sonnet-thinking` | claude-sonnet-4-5-thinking |
| `flash` | gemini-2.5-flash |
| `pro` | gemini-2.5-pro |
| `3-flash` | gemini-3-flash |
| `3-pro` | gemini-3-pro-high |

## Supported Models

### Claude Models
- `claude-opus-4-6-thinking`
- `claude-opus-4-5-thinking`
- `claude-sonnet-4-5`
- `claude-sonnet-4-5-thinking`

### Gemini Models
- `gemini-2.5-flash`
- `gemini-2.5-flash-lite`
- `gemini-2.5-flash-thinking`
- `gemini-2.5-pro`
- `gemini-3-flash`
- `gemini-3-pro-high`
- `gemini-3-pro-image`
- `gemini-3-pro-low`

## Configuration

AGCP uses a TOML configuration file at `~/.config/agcp/config.toml`:

```toml
[server]
port = 8080
host = "127.0.0.1"
# api_key = "your-optional-api-key"
request_timeout_secs = 300       # Per-request timeout (default: 5 minutes)

[logging]
debug = false
log_requests = false

[accounts]
strategy = "hybrid"      # "sticky", "roundrobin", or "hybrid"
quota_threshold = 0.1    # Deprioritize accounts below 10% quota
fallback = false

[cache]
enabled = true
ttl_seconds = 300
max_entries = 100

[cloudcode]
timeout_secs = 120
max_retries = 5
max_concurrent_requests = 1      # Max parallel requests to Cloud Code API
min_request_interval_ms = 500    # Minimum delay between requests (ms)
```

### Account Selection Strategies

- **`sticky`** - Use the same account until it hits quota limits
- **`roundrobin`** - Rotate through accounts evenly
- **`hybrid`** - Smart selection based on account health and quota (recommended)

## Multi-Account Management

AGCP supports multiple Google accounts for higher throughput:

```bash
# Add accounts
agcp login                    # Add first account
agcp login                    # Add another account

# View accounts
agcp accounts                 # List all accounts

# Manage accounts
agcp accounts disable <id>    # Disable an account
agcp accounts enable <id>     # Re-enable an account
agcp accounts remove <id>     # Remove an account
```

## API Endpoints

| Endpoint | Description |
|----------|-------------|
| `POST /v1/messages` | Anthropic Messages API (streaming and non-streaming) |
| `GET /v1/models` | List available models |
| `GET /health` | Health check |
| `GET /stats` | Server and cache statistics |

## Response Caching

AGCP caches non-streaming responses to reduce API quota usage:

- Identical requests return cached responses instantly
- Streaming and thinking model responses are not cached
- Use `X-No-Cache: true` header to bypass cache
- Cache headers: `X-Cache: HIT`, `X-Cache: MISS`, `X-Cache: BYPASS`

## Configuring AI Tools

### Claude Code

```bash
agcp setup
```

Select "Claude Code" from the interactive menu, or manually add to `~/.claude/settings.json`:

```json
{
  "apiBaseUrl": "http://127.0.0.1:8080"
}
```

### Other Tools

Point any Anthropic API-compatible tool to `http://127.0.0.1:8080/v1`.

## Troubleshooting

```bash
agcp doctor    # Run diagnostic checks
agcp status    # Quick status check
agcp logs      # View logs
```

## Files

| Path | Description |
|------|-------------|
| `~/.config/agcp/config.toml` | Configuration file |
| `~/.config/agcp/accounts.json` | Account credentials |
| `~/.config/agcp/agcp.log` | Server logs |

## License

MIT - See [LICENSE](LICENSE) for details.

---

Made with Rust
