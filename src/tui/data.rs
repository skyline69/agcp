//! Data types for TUI display

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex_lite::Regex;

/// Regex to strip ANSI escape codes
static ANSI_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").expect("valid regex"));

/// Regex to match request completed log lines and extract timestamp
/// Format: "2026-02-05T21:25:01.034804Z  INFO Request completed method=POST path=/messages"
/// Also matches path=/v1/messages for compatibility
static REQUEST_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})\.\d+Z\s+INFO\s+Request completed.*path=(/v1)?/messages")
        .expect("valid regex")
});

/// Regex to match "Server listening" log line to determine daemon start time
/// Format: "2026-02-05T12:39:09.607047Z  INFO Server listening address=127.0.0.1:3092"
static SERVER_START_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})\.\d+Z\s+INFO\s+Server listening")
        .expect("valid regex")
});

/// Regex to match "Model used" log line and extract model name
/// Format: "2026-02-06T11:53:09.123456Z  INFO Model used model=claude-sonnet-4-20250514 request_id=req_xxx"
static MODEL_USED_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"INFO\s+Model used\s+model=([^\s]+)").expect("valid regex"));

/// Regex to extract duration_ms from request completed log lines
static DURATION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"duration_ms=(\d+)").expect("valid regex"));

/// Regex to extract account email from "Model used" log lines
/// Format: "account=user@example.com"
static ACCOUNT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"account=([^\s]+)").expect("valid regex"));

/// Server status for display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Running,
    Stopped,
}

impl ServerStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, ServerStatus::Running)
    }
}

/// Account info for display
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub id: String,
    pub email: String,
    pub quota_fraction: f64,
    pub is_active: bool,
    pub enabled: bool,
    pub is_invalid: bool,
    pub subscription_tier: Option<String>,
}

/// Model usage statistics
#[derive(Debug, Clone)]
pub struct ModelUsage {
    pub model: String,
    pub requests: u64,
}

/// Log level for display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Parse log level from log line
    /// tracing format examples after ANSI strip:
    /// "2026-02-05T12:36:52Z  INFO message"
    /// "2026-02-05T12:36:52Z ERROR message"  
    /// "2026-02-05T12:36:52Z  WARN message"
    pub fn parse(line: &str) -> Self {
        // Check for level keywords - they appear after timestamp, before message
        // Use simple contains since spacing varies
        if line.contains(" DEBUG ") {
            LogLevel::Debug
        } else if line.contains(" WARN ") || line.contains("WARN ") {
            LogLevel::Warn
        } else if line.contains(" ERROR ") || line.contains("ERROR ") {
            LogLevel::Error
        } else if line.contains(" INFO ") || line.contains("INFO ") {
            LogLevel::Info
        } else {
            // Default to Info for unrecognized format
            LogLevel::Info
        }
    }
}

/// Log entry for display
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub line: String,
    pub level: LogLevel,
    /// Timestamp in seconds since epoch (for rate calculations)
    pub timestamp_secs: Option<u64>,
    /// Whether this is a request completion
    pub is_request: bool,
    /// Account email extracted from "Model used" log lines
    pub account_email: Option<String>,
}

impl LogEntry {
    pub fn new(line: String) -> Self {
        // Strip ANSI escape codes for clean display
        let clean_line = ANSI_REGEX.replace_all(&line, "").to_string();
        let level = LogLevel::parse(&clean_line);

        // Check if this is a request completion and extract timestamp
        let (timestamp_secs, is_request) = if let Some(caps) = REQUEST_REGEX.captures(&clean_line) {
            let ts = caps.get(1).and_then(|m| parse_timestamp(m.as_str()));
            (ts, true)
        } else {
            (None, false)
        };

        // Extract account email from "Model used" lines
        let account_email = ACCOUNT_REGEX
            .captures(&clean_line)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string());

        Self {
            line: clean_line,
            level,
            timestamp_secs,
            is_request,
            account_email,
        }
    }
}

/// Parse timestamp "2026-02-05T21:25:01" to seconds since epoch
fn parse_timestamp(s: &str) -> Option<u64> {
    // Simple parsing: YYYY-MM-DDTHH:MM:SS
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return None;
    }

    let date_parts: Vec<u32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u32> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();

    if date_parts.len() != 3 || time_parts.len() != 3 {
        return None;
    }

    // Calculate seconds since a reference point (Jan 1, 2020)
    // This doesn't need to be accurate epoch time, just consistent for rate calculation
    let year = date_parts[0];
    let month = date_parts[1];
    let day = date_parts[2];
    let hour = time_parts[0];
    let min = time_parts[1];
    let sec = time_parts[2];

    // Approximate days since 2020-01-01
    let years_since_2020 = year.saturating_sub(2020);
    let days = years_since_2020 * 365 + (month - 1) * 30 + day;
    let secs = (days as u64) * 86400 + (hour as u64) * 3600 + (min as u64) * 60 + (sec as u64);

    Some(secs)
}

/// Build rate history from log entries (requests per second for last 60 seconds)
/// Accepts pre-computed `now` timestamp to avoid redundant computation
pub fn build_rate_history(logs: &VecDeque<LogEntry>, now: u64) -> Vec<u64> {
    use std::collections::HashMap;

    // Always return 60 entries (even if empty) so chart renders
    if now == 0 {
        return vec![0; 60];
    }

    // Count requests per second in the last 60 seconds from NOW
    let mut counts: HashMap<u64, u64> = HashMap::new();
    for entry in logs.iter() {
        if entry.is_request
            && let Some(ts) = entry.timestamp_secs
            && ts > now.saturating_sub(60)
            && ts <= now
        {
            *counts.entry(ts).or_insert(0) += 1;
        }
    }

    // Build the result vector (oldest to newest, always 60 entries)
    let start_ts = now.saturating_sub(59);
    (0..60)
        .map(|i| counts.get(&(start_ts + i)).copied().unwrap_or(0))
        .collect()
}

/// Count total requests and model usage from logs
pub fn count_requests_from_logs(logs: &VecDeque<LogEntry>) -> (u64, Vec<ModelUsage>) {
    use std::collections::HashMap;

    let total = logs.iter().filter(|e| e.is_request).count() as u64;

    // Extract model info from "Model used" log lines
    let mut models: HashMap<String, u64> = HashMap::new();
    for entry in logs.iter() {
        if let Some(caps) = MODEL_USED_REGEX.captures(&entry.line)
            && let Some(model) = caps.get(1)
        {
            *models.entry(model.as_str().to_string()).or_insert(0) += 1;
        }
    }

    let mut model_usage: Vec<ModelUsage> = models
        .into_iter()
        .map(|(model, requests)| ModelUsage { model, requests })
        .collect();

    // Sort by request count descending, then by model name for stability
    model_usage.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.model.cmp(&b.model))
    });

    (total, model_usage)
}

/// Calculate average response time from logs (in milliseconds)
pub fn calculate_avg_response_time(logs: &VecDeque<LogEntry>) -> Option<u64> {
    let mut total_ms: u64 = 0;
    let mut count: u64 = 0;

    for entry in logs.iter() {
        if entry.is_request
            && let Some(caps) = DURATION_REGEX.captures(&entry.line)
            && let Some(ms_str) = caps.get(1)
            && let Ok(ms) = ms_str.as_str().parse::<u64>()
        {
            total_ms += ms;
            count += 1;
        }
    }

    if count > 0 {
        Some(total_ms / count)
    } else {
        None
    }
}

/// Calculate requests per minute based on recent log activity
/// Accepts pre-computed `now` timestamp to avoid redundant computation
pub fn calculate_requests_per_min(logs: &VecDeque<LogEntry>, now: u64) -> f64 {
    let one_min_ago = now.saturating_sub(60);

    let recent_count = logs
        .iter()
        .filter(|e| e.is_request && e.timestamp_secs.is_some_and(|ts| ts >= one_min_ago))
        .count();

    recent_count as f64
}

/// Find the daemon's start time from logs (looks for "Server listening" message)
/// Returns the timestamp of the most recent server start
pub fn find_daemon_start_time(logs: &VecDeque<LogEntry>) -> Option<u64> {
    // Search backwards to find the most recent "Server listening" log
    for entry in logs.iter().rev() {
        if let Some(caps) = SERVER_START_REGEX.captures(&entry.line)
            && let Some(ts) = caps.get(1).and_then(|m| parse_timestamp(m.as_str()))
        {
            return Some(ts);
        }
    }
    None
}

/// Parse daemon start time from a raw log line (for use with find_server_start_line)
pub fn parse_daemon_start_from_line(line: &str) -> Option<u64> {
    // Strip ANSI codes first
    let clean_line = ANSI_REGEX.replace_all(line, "").to_string();
    if let Some(caps) = SERVER_START_REGEX.captures(&clean_line) {
        return caps.get(1).and_then(|m| parse_timestamp(m.as_str()));
    }
    None
}

/// Get current time as seconds (same format as parse_timestamp)
pub fn current_time_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Convert Unix timestamp to UTC components
    // This must exactly match parse_timestamp's calculation

    const SECS_PER_MIN: u64 = 60;
    const SECS_PER_HOUR: u64 = 3600;
    const SECS_PER_DAY: u64 = 86400;

    // Days and time of day
    let days_since_1970 = now / SECS_PER_DAY;
    let time_of_day = now % SECS_PER_DAY;
    let hour = (time_of_day / SECS_PER_HOUR) as u32;
    let min = ((time_of_day % SECS_PER_HOUR) / SECS_PER_MIN) as u32;
    let sec = (time_of_day % SECS_PER_MIN) as u32;

    // Convert days since 1970 to year/month/day
    // Using the same approximate logic as parse_timestamp
    let mut remaining_days = days_since_1970 as i64;
    let mut year = 1970u32;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    // Now remaining_days is day of year (0-based)
    let day_of_year = remaining_days as u32;

    // Convert to month/day (1-based)
    let (month, day) = day_of_year_to_month_day(day_of_year, is_leap_year(year));

    // Now use EXACTLY the same formula as parse_timestamp
    let years_since_2020 = year.saturating_sub(2020);
    let days = years_since_2020 * 365 + (month - 1) * 30 + day;
    (days as u64) * 86400 + (hour as u64) * 3600 + (min as u64) * 60 + (sec as u64)
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn day_of_year_to_month_day(day_of_year: u32, leap: bool) -> (u32, u32) {
    let days_in_months: [u32; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut remaining = day_of_year;
    for (i, &days) in days_in_months.iter().enumerate() {
        if remaining < days {
            return ((i + 1) as u32, remaining + 1);
        }
        remaining -= days;
    }
    (12, 31) // Fallback to Dec 31
}

/// Data provider for the TUI
pub struct DataProvider;

impl Default for DataProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DataProvider {
    pub fn new() -> Self {
        Self
    }

    /// Check if the AGCP daemon is actually running by checking the PID file.
    /// A TCP/HTTP probe is insufficient because other apps (e.g. Antigravity.app)
    /// may also be listening on the same port with a compatible health endpoint.
    pub fn get_server_status(&self) -> ServerStatus {
        let pid_path = crate::config::Config::dir().join("agcp.pid");

        // Read PID from file
        let pid_str = match std::fs::read_to_string(&pid_path) {
            Ok(s) => s,
            Err(_) => return ServerStatus::Stopped, // No PID file = not running
        };

        let _pid: i32 = match pid_str.trim().parse() {
            Ok(p) => p,
            Err(_) => return ServerStatus::Stopped, // Invalid PID file
        };

        // Check if the process is actually alive (signal 0 = existence check)
        #[cfg(unix)]
        {
            if unsafe { libc::kill(_pid, 0) } == 0 {
                ServerStatus::Running
            } else {
                ServerStatus::Stopped
            }
        }

        #[cfg(not(unix))]
        {
            // On non-Unix, fall back to HTTP health check
            use std::io::{Read, Write};
            use std::net::TcpStream;
            use std::time::Duration;

            let config = crate::config::get_config();
            let addr = format!("{}:{}", config.server.host, config.server.port);
            let sock_addr = match addr.parse() {
                Ok(a) => a,
                Err(_) => return ServerStatus::Stopped,
            };
            let mut stream =
                match TcpStream::connect_timeout(&sock_addr, Duration::from_millis(200)) {
                    Ok(s) => s,
                    Err(_) => return ServerStatus::Stopped,
                };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            let request = format!(
                "GET /health HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                addr
            );
            if stream.write_all(request.as_bytes()).is_err() {
                return ServerStatus::Stopped;
            }
            let mut buf = [0u8; 512];
            let n = match stream.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return ServerStatus::Stopped,
            };
            let response = String::from_utf8_lossy(&buf[..n]);
            if response.contains(r#""status":"ok""#) {
                ServerStatus::Running
            } else {
                ServerStatus::Stopped
            }
        }
    }

    /// Get list of accounts
    pub fn get_accounts(&self) -> Vec<AccountInfo> {
        match crate::auth::accounts::AccountStore::load() {
            Ok(store) => store
                .accounts
                .iter()
                .map(|acc| AccountInfo {
                    id: acc.id.clone(),
                    email: acc.email.clone(),
                    quota_fraction: acc.get_average_quota_fraction(),
                    is_active: store.active_account_id.as_ref() == Some(&acc.id),
                    enabled: acc.enabled,
                    is_invalid: acc.is_invalid,
                    subscription_tier: acc.subscription_tier.clone(),
                })
                .collect(),
            Err(_) => vec![],
        }
    }

    /// Get the log file path
    pub fn get_log_path() -> PathBuf {
        crate::config::Config::dir().join("agcp.log")
    }

    /// Fetch token usage stats from the running server's /stats endpoint.
    /// Returns None if the server is not running or the request fails.
    pub fn fetch_token_stats() -> Option<TokenStats> {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let config = crate::config::get_config();
        let addr = format!("{}:{}", config.server.host, config.server.port);

        let mut stream = TcpStream::connect_timeout(
            &addr.parse().ok()?,
            Duration::from_millis(500),
        )
        .ok()?;
        stream.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
        stream.set_write_timeout(Some(Duration::from_millis(500))).ok()?;

        let request = format!(
            "GET /stats HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            addr
        );
        stream.write_all(request.as_bytes()).ok()?;

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).ok()?;

        let response = String::from_utf8_lossy(&buf);
        // Find JSON body after HTTP headers
        let json_start = response.find("\r\n\r\n")? + 4;
        let body = &response[json_start..];
        // Handle chunked transfer encoding — find the JSON object
        let json_str = if let Some(start) = body.find('{') {
            // Find the matching closing brace
            let sub = &body[start..];
            // Use the last '}' in the response as the end
            let end = sub.rfind('}')?;
            &sub[..=end]
        } else {
            return None;
        };

        let json: serde_json::Value = serde_json::from_str(json_str).ok()?;
        let requests = &json["requests"];

        // Parse per-model token stats
        let mut models = Vec::new();
        if let Some(model_arr) = requests["models"].as_array() {
            for m in model_arr {
                let input = m["input_tokens"].as_u64().unwrap_or(0);
                let output = m["output_tokens"].as_u64().unwrap_or(0);
                let cache = m["cache_read_tokens"].as_u64().unwrap_or(0);
                if input > 0 || output > 0 {
                    models.push(ModelTokenStats {
                        model: m["model"].as_str().unwrap_or("unknown").to_string(),
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: cache,
                    });
                }
            }
        }

        // Parse totals
        let token_usage = &requests["token_usage"];
        let total_input = token_usage["total_input_tokens"].as_u64().unwrap_or(0);
        let total_output = token_usage["total_output_tokens"].as_u64().unwrap_or(0);
        let total_cache = token_usage["total_cache_read_tokens"].as_u64().unwrap_or(0);

        Some(TokenStats {
            models,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read_tokens: total_cache,
        })
    }
}

/// Per-model token usage statistics
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ModelTokenStats {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

/// Aggregated token usage stats from the server
#[derive(Debug, Clone)]
pub struct TokenStats {
    pub models: Vec<ModelTokenStats>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
}

/// Maximum number of data points to keep in the token history
const TOKEN_HISTORY_MAX_POINTS: usize = 2000;

/// A single timestamped snapshot of cumulative token usage
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenSnapshot {
    /// Unix timestamp (seconds since epoch)
    pub timestamp: u64,
    /// Per-model cumulative totals: (model_name, total_tokens)
    pub models: Vec<(String, u64)>,
}

/// Persistent time-series of cumulative token usage per model.
/// Snapshots are taken every poll interval and persisted to disk
/// so they survive TUI/server restarts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenHistory {
    /// Ordered list of snapshots
    pub snapshots: Vec<TokenSnapshot>,
    /// Unix timestamp of the quota period start (set from quota reset_time - 24h)
    #[serde(default)]
    pub period_start: Option<u64>,
}

impl Default for TokenHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenHistory {
    pub fn new() -> Self {
        Self {
            snapshots: Vec::new(),
            period_start: None,
        }
    }

    /// Load from the persistence file, or return a new empty history
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::new(),
        }
    }

    /// Save to the persistence file
    pub fn save(&self) {
        let path = Self::path();
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Path to the persistence file
    fn path() -> std::path::PathBuf {
        crate::config::Config::dir().join("token_history.json")
    }

    /// Record a new snapshot from the current TokenStats.
    /// Returns true if the snapshot was added (i.e., there's new data).
    pub fn push(&mut self, stats: &TokenStats) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let models: Vec<(String, u64)> = stats
            .models
            .iter()
            .map(|m| (m.model.clone(), m.input_tokens + m.output_tokens))
            .collect();

        // Skip if no token data
        if models.is_empty() || models.iter().all(|(_, t)| *t == 0) {
            return false;
        }

        self.snapshots.push(TokenSnapshot {
            timestamp: now,
            models,
        });

        // Trim old snapshots
        if self.snapshots.len() > TOKEN_HISTORY_MAX_POINTS {
            let excess = self.snapshots.len() - TOKEN_HISTORY_MAX_POINTS;
            self.snapshots.drain(..excess);
        }

        true
    }

    /// Update the quota period start time from quota data.
    /// Assumes a 24-hour quota period ending at reset_time.
    pub fn set_period_from_reset_time(&mut self, reset_time_str: &str) {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_time_str) {
            let reset_ts = dt.timestamp() as u64;
            // Quota periods are typically 24 hours
            let period_start = reset_ts.saturating_sub(86400);
            self.period_start = Some(period_start);
        }
    }

    /// Reset the history (clear all snapshots). Called when quota period resets.
    pub fn reset(&mut self) {
        self.snapshots.clear();
        self.save();
    }

    /// Check if the quota period has ended (current time > reset_time).
    /// If reset_time is in the past, we should reset.
    pub fn should_reset(&self, reset_time_str: &str) -> bool {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_time_str) {
            let reset_ts = dt.timestamp() as u64;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Only reset if we have snapshots that are from before the reset time
            // and the reset has already happened
            if now >= reset_ts
                && let Some(first) = self.snapshots.first()
            {
                return first.timestamp < reset_ts;
            }
        }
        false
    }

    /// Get per-model cumulative data for the chart.
    /// Returns Vec of (model_name, Vec<(x, y)>) where:
    /// - x = minutes since period_start (or first snapshot)
    /// - y = cumulative total tokens
    pub fn get_cumulative_series(&self) -> Vec<(String, Vec<(f64, f64)>)> {
        if self.snapshots.is_empty() {
            return Vec::new();
        }

        let time_origin = self
            .period_start
            .unwrap_or_else(|| self.snapshots[0].timestamp);

        // Collect all unique model names
        let mut model_names: Vec<String> = Vec::new();
        for snap in &self.snapshots {
            for (name, _) in &snap.models {
                if !model_names.contains(name) {
                    model_names.push(name.clone());
                }
            }
        }

        let mut result = Vec::new();
        for model_name in &model_names {
            let mut points = Vec::new();
            for snap in &self.snapshots {
                let x = (snap.timestamp.saturating_sub(time_origin)) as f64 / 60.0; // minutes
                let y = snap
                    .models
                    .iter()
                    .find(|(n, _)| n == model_name)
                    .map(|(_, t)| *t as f64)
                    .unwrap_or(0.0);
                points.push((x, y));
            }
            // Only include models with non-zero data
            if points.iter().any(|(_, y)| *y > 0.0) {
                result.push((model_name.clone(), points));
            }
        }
        result
    }

    /// Get the time range in minutes for the X axis
    pub fn get_time_range_minutes(&self) -> f64 {
        if self.snapshots.is_empty() {
            return 60.0; // Default 1 hour
        }

        let time_origin = self
            .period_start
            .unwrap_or_else(|| self.snapshots[0].timestamp);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        (now.saturating_sub(time_origin)) as f64 / 60.0
    }
}
