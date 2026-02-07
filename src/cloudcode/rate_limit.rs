use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

pub const RATE_LIMIT_DEDUP_WINDOW_MS: u64 = 2000;
pub const RATE_LIMIT_STATE_RESET_MS: u64 = 120_000;
pub const FIRST_RETRY_DELAY_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 60_000;
pub const MIN_BACKOFF_MS: u64 = 2000;
pub const MAX_WAIT_BEFORE_ERROR_MS: u64 = 120_000;
pub const DEFAULT_COOLDOWN_MS: u64 = 10_000;

pub mod backoff_by_error_type {
    pub const RATE_LIMIT_EXCEEDED: u64 = 30_000;
    pub const MODEL_CAPACITY_EXHAUSTED: u64 = 15_000;
    pub const SERVER_ERROR: u64 = 20_000;
    pub const UNKNOWN: u64 = 60_000;
}

pub const CAPACITY_BACKOFF_TIERS_MS: [u64; 5] = [5000, 10000, 20000, 30000, 60000];
pub const MAX_CAPACITY_RETRIES: u32 = 5;
pub const QUOTA_EXHAUSTED_BACKOFF_TIERS_MS: [u64; 4] = [60_000, 300_000, 1_800_000, 7_200_000];

/// Pre-compiled regex for parsing quotaResetDelay from error messages
static QUOTA_RESET_DELAY_REGEX: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r#"quotaresetdelay[:\s"]+([\d.]+)(ms|s)"#)
        .expect("Invalid quotaResetDelay regex")
});

/// Pre-compiled regex for parsing quotaResetTimestamp from error messages
static QUOTA_RESET_TIMESTAMP_REGEX: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r#"quotaresettimestamp[:\s"]+(\d{4}-\d{2}-\d{2}T[\d:.]+Z?)"#)
        .expect("Invalid quotaResetTimestamp regex")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitReason {
    QuotaExhausted,
    ModelCapacityExhausted,
    RateLimitExceeded,
    ServerError,
    Unknown,
}

pub fn parse_rate_limit_reason(error_text: &str) -> RateLimitReason {
    let lower = error_text.to_lowercase();

    if lower.contains("quota_exhausted")
        || lower.contains("quotaresetdelay")
        || lower.contains("quotaresettimestamp")
        || lower.contains("resource_exhausted")
        || lower.contains("daily limit")
        || lower.contains("quota exceeded")
    {
        return RateLimitReason::QuotaExhausted;
    }

    if lower.contains("model_capacity_exhausted")
        || lower.contains("capacity_exhausted")
        || lower.contains("model is currently overloaded")
        || lower.contains("service temporarily unavailable")
    {
        return RateLimitReason::ModelCapacityExhausted;
    }

    if lower.contains("rate_limit_exceeded")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("throttl")
    {
        return RateLimitReason::RateLimitExceeded;
    }

    if lower.contains("internal server error")
        || lower.contains("server error")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("504")
    {
        return RateLimitReason::ServerError;
    }

    RateLimitReason::Unknown
}

pub fn is_model_capacity_exhausted(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("model_capacity_exhausted")
        || lower.contains("capacity_exhausted")
        || lower.contains("model is currently overloaded")
        || lower.contains("service temporarily unavailable")
}

#[derive(Debug, Clone)]
struct RateLimitState {
    consecutive_429: u32,
    last_at: Instant,
}

static RATE_LIMIT_STATE: LazyLock<RwLock<HashMap<String, RateLimitState>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[derive(Debug)]
pub struct RateLimitBackoff {
    pub attempt: u32,
    pub delay_ms: u64,
    pub is_duplicate: bool,
}

/// Prevents thundering herd when multiple concurrent requests hit 429
pub fn get_rate_limit_backoff(model: &str, server_retry_after_ms: Option<u64>) -> RateLimitBackoff {
    let now = Instant::now();

    {
        let state_map = RATE_LIMIT_STATE.read();
        if let Some(state) = state_map.get(model) {
            let elapsed_ms = state.last_at.elapsed().as_millis() as u64;

            if elapsed_ms < RATE_LIMIT_DEDUP_WINDOW_MS {
                let base_delay = server_retry_after_ms.unwrap_or(FIRST_RETRY_DELAY_MS);
                let backoff_delay = calculate_backoff(base_delay, state.consecutive_429);

                tracing::debug!(
                    model = %model,
                    attempt = state.consecutive_429,
                    delay_ms = backoff_delay,
                    "Rate limit within dedup window, isDuplicate=true"
                );

                return RateLimitBackoff {
                    attempt: state.consecutive_429,
                    delay_ms: backoff_delay,
                    is_duplicate: true,
                };
            }
        }
    }

    let mut state_map = RATE_LIMIT_STATE.write();

    let attempt = if let Some(state) = state_map.get(model) {
        let elapsed_ms = state.last_at.elapsed().as_millis() as u64;
        if elapsed_ms < RATE_LIMIT_STATE_RESET_MS {
            state.consecutive_429 + 1
        } else {
            1
        }
    } else {
        1
    };

    state_map.insert(
        model.to_string(),
        RateLimitState {
            consecutive_429: attempt,
            last_at: now,
        },
    );

    let base_delay = server_retry_after_ms.unwrap_or(FIRST_RETRY_DELAY_MS);
    let backoff_delay = calculate_backoff(base_delay, attempt);

    tracing::debug!(
        model = %model,
        attempt = attempt,
        delay_ms = backoff_delay,
        "Rate limit backoff calculated"
    );

    RateLimitBackoff {
        attempt,
        delay_ms: backoff_delay,
        is_duplicate: false,
    }
}

pub fn clear_rate_limit_state(model: &str) {
    let mut state_map = RATE_LIMIT_STATE.write();
    if state_map.remove(model).is_some() {
        tracing::debug!(model = %model, "Cleared rate limit state after success");
    }
}

fn calculate_backoff(base_delay: u64, attempt: u32) -> u64 {
    let multiplier = 2u64.saturating_pow(attempt.saturating_sub(1));
    let delay = base_delay.saturating_mul(multiplier);
    delay.min(MAX_BACKOFF_MS).max(base_delay)
}

pub fn calculate_smart_backoff(
    error_text: &str,
    server_reset_ms: Option<u64>,
    consecutive_failures: u32,
) -> u64 {
    if let Some(reset_ms) = server_reset_ms
        && reset_ms > 0
    {
        return reset_ms.max(MIN_BACKOFF_MS);
    }

    let reason = parse_rate_limit_reason(error_text);

    match reason {
        RateLimitReason::QuotaExhausted => {
            let tier_index =
                (consecutive_failures as usize).min(QUOTA_EXHAUSTED_BACKOFF_TIERS_MS.len() - 1);
            QUOTA_EXHAUSTED_BACKOFF_TIERS_MS[tier_index]
        }
        RateLimitReason::RateLimitExceeded => backoff_by_error_type::RATE_LIMIT_EXCEEDED,
        RateLimitReason::ModelCapacityExhausted => backoff_by_error_type::MODEL_CAPACITY_EXHAUSTED,
        RateLimitReason::ServerError => backoff_by_error_type::SERVER_ERROR,
        RateLimitReason::Unknown => backoff_by_error_type::UNKNOWN,
    }
}

pub fn parse_reset_time(error_body: &str, default_ms: u64) -> (u64, String) {
    let lower = error_body.to_lowercase();

    let mut reset_ms: Option<u64> = None;

    if reset_ms.is_none()
        && let Some(ms) = parse_quota_reset_delay(&lower)
    {
        reset_ms = Some(ms);
        tracing::debug!(reset_ms = ms, "Parsed quotaResetDelay from body");
    }

    if reset_ms.is_none()
        && let Some(ms) = parse_quota_reset_timestamp(&lower)
    {
        reset_ms = Some(ms);
        tracing::debug!(reset_ms = ms, "Parsed quotaResetTimeStamp from body");
    }

    if reset_ms.is_none()
        && let Some(ms) = parse_duration_string(&lower)
    {
        reset_ms = Some(ms);
        tracing::debug!(reset_ms = ms, "Parsed duration from body");
    }

    if reset_ms.is_none()
        && let Some(ms) = parse_retry_after(&lower)
    {
        reset_ms = Some(ms);
        tracing::debug!(reset_ms = ms, "Parsed retry-after from body");
    }

    let final_ms = match reset_ms {
        Some(0) => {
            tracing::debug!("Reset time invalid (0ms), using 500ms default");
            500
        }
        Some(ms) if ms < 500 => {
            tracing::debug!(ms = ms, "Short reset time, adding 200ms buffer");
            ms + 200
        }
        Some(ms) => ms,
        None => default_ms,
    };

    (final_ms, format_duration(final_ms))
}

fn parse_quota_reset_delay(text: &str) -> Option<u64> {
    if let Some(captures) = QUOTA_RESET_DELAY_REGEX.captures(text) {
        let value: f64 = captures.get(1)?.as_str().parse().ok()?;
        let unit = captures.get(2)?.as_str();

        let ms = if unit == "s" {
            (value * 1000.0).ceil() as u64
        } else {
            value.ceil() as u64
        };

        return Some(ms);
    }

    None
}

fn parse_quota_reset_timestamp(text: &str) -> Option<u64> {
    if let Some(captures) = QUOTA_RESET_TIMESTAMP_REGEX.captures(text) {
        let timestamp_str = captures.get(1)?.as_str();
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp_str) {
            let now = chrono::Utc::now();
            let delta = parsed.signed_duration_since(now);
            if delta.num_milliseconds() > 0 {
                return Some(delta.num_milliseconds() as u64);
            }
            return Some(500);
        }
    }

    None
}

fn parse_duration_string(text: &str) -> Option<u64> {
    let mut total_ms = 0u64;
    let mut found = false;

    if let Some(pos) = text.find('h')
        && pos > 0
    {
        let start = text[..pos]
            .rfind(|c: char| !c.is_ascii_digit())
            .map(|p| p + 1)
            .unwrap_or(0);
        if let Ok(hours) = text[start..pos].parse::<u64>() {
            total_ms += hours * 3600 * 1000;
            found = true;
        }
    }

    if let Some(pos) = text.find('m') {
        if pos + 1 < text.len() && text.as_bytes().get(pos + 1) == Some(&b's') {
        } else if pos > 0 {
            let start = text[..pos]
                .rfind(|c: char| !c.is_ascii_digit())
                .map(|p| p + 1)
                .unwrap_or(0);
            if let Ok(mins) = text[start..pos].parse::<u64>() {
                total_ms += mins * 60 * 1000;
                found = true;
            }
        }
    }

    for (i, c) in text.char_indices() {
        if c == 's' && i > 0 {
            if i >= 1 && text.as_bytes().get(i - 1) == Some(&b'm') {
                continue;
            }
            let start = text[..i]
                .rfind(|c: char| !c.is_ascii_digit())
                .map(|p| p + 1)
                .unwrap_or(0);
            if start < i
                && let Ok(secs) = text[start..i].parse::<u64>()
            {
                total_ms += secs * 1000;
                found = true;
                break;
            }
        }
    }

    if found { Some(total_ms) } else { None }
}

fn parse_retry_after(text: &str) -> Option<u64> {
    if let Some(pos) = text.find("retry") {
        let after = &text[pos + 5..];
        let start = after.find(|c: char| c.is_ascii_digit())?;
        let after = &after[start..];
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        let secs: u64 = after[..end].parse().ok()?;
        Some(secs * 1000)
    } else {
        None
    }
}

pub fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    let mut result = String::new();
    if hours > 0 {
        result.push_str(&hours.to_string());
        result.push('h');
        result.push_str(&mins.to_string());
        result.push('m');
        result.push_str(&secs.to_string());
        result.push('s');
    } else if mins > 0 {
        result.push_str(&mins.to_string());
        result.push('m');
        result.push_str(&secs.to_string());
        result.push('s');
    } else {
        result.push_str(&secs.to_string());
        result.push('s');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(5000), "5s");
        assert_eq!(format_duration(65000), "1m5s");
        assert_eq!(format_duration(3665000), "1h1m5s");
    }

    #[test]
    fn test_parse_duration_string() {
        assert_eq!(parse_duration_string("5m0s"), Some(300000));
        assert_eq!(parse_duration_string("45s"), Some(45000));
        assert_eq!(parse_duration_string("1h23m45s"), Some(5025000));
    }

    #[test]
    fn test_calculate_backoff() {
        assert_eq!(calculate_backoff(1000, 1), 1000); // 1000 * 2^0 = 1000
        assert_eq!(calculate_backoff(1000, 2), 2000); // 1000 * 2^1 = 2000
        assert_eq!(calculate_backoff(1000, 3), 4000); // 1000 * 2^2 = 4000
        assert_eq!(calculate_backoff(1000, 7), 60000); // Capped at 60000
    }

    #[test]
    fn test_rate_limit_backoff() {
        let model = "test-model-backoff";

        // First rate limit
        let result1 = get_rate_limit_backoff(model, None);
        assert_eq!(result1.attempt, 1);
        assert!(!result1.is_duplicate);

        // Immediate second call should be duplicate
        let result2 = get_rate_limit_backoff(model, None);
        assert_eq!(result2.attempt, 1); // Same attempt since it's a duplicate
        assert!(result2.is_duplicate);

        // Clear state
        clear_rate_limit_state(model);
    }

    #[test]
    fn test_parse_rate_limit_reason() {
        assert_eq!(
            parse_rate_limit_reason("QUOTA_EXHAUSTED: daily limit"),
            RateLimitReason::QuotaExhausted
        );
        assert_eq!(
            parse_rate_limit_reason("model_capacity_exhausted"),
            RateLimitReason::ModelCapacityExhausted
        );
        assert_eq!(
            parse_rate_limit_reason("rate_limit_exceeded"),
            RateLimitReason::RateLimitExceeded
        );
        assert_eq!(
            parse_rate_limit_reason("internal server error"),
            RateLimitReason::ServerError
        );
        assert_eq!(
            parse_rate_limit_reason("something else"),
            RateLimitReason::Unknown
        );
    }

    #[test]
    fn test_calculate_smart_backoff() {
        // Server-provided reset time should be used
        assert_eq!(calculate_smart_backoff("error", Some(5000), 0), 5000);

        // Small server-provided reset should be clamped to MIN_BACKOFF_MS
        assert_eq!(calculate_smart_backoff("error", Some(500), 0), 2000);

        // Quota exhausted should use progressive tiers
        assert_eq!(calculate_smart_backoff("quota_exhausted", None, 0), 60000);
        assert_eq!(calculate_smart_backoff("quota_exhausted", None, 1), 300000);
    }

    #[test]
    fn test_is_model_capacity_exhausted() {
        assert!(is_model_capacity_exhausted("model_capacity_exhausted"));
        assert!(is_model_capacity_exhausted("capacity_exhausted"));
        assert!(is_model_capacity_exhausted("model is currently overloaded"));
        assert!(!is_model_capacity_exhausted("quota_exhausted"));
    }
}
