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

    /// Check if server is actually running by probing the configured port
    pub fn get_server_status(&self) -> ServerStatus {
        use std::net::TcpStream;
        use std::time::Duration;

        let config = crate::config::get_config();
        let addr = format!("{}:{}", config.server.host, config.server.port);

        match addr.parse() {
            Ok(sock_addr) => {
                match TcpStream::connect_timeout(&sock_addr, Duration::from_millis(100)) {
                    Ok(_) => ServerStatus::Running,
                    Err(_) => ServerStatus::Stopped,
                }
            }
            Err(_) => ServerStatus::Stopped,
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
                    quota_fraction: acc.get_quota_fraction("default"),
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
}
