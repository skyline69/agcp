use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Number of seconds to track for the request rate graph
const RATE_HISTORY_SIZE: usize = 60;

/// Maximum number of token events to keep for time-series display
const MAX_TOKEN_EVENTS: usize = 1000;

/// Global stats instance
static STATS: std::sync::LazyLock<Stats> = std::sync::LazyLock::new(Stats::new);

/// Get the global stats instance
pub fn get_stats() -> &'static Stats {
    &STATS
}

/// Path to the persistent stats file.
fn stats_path() -> std::path::PathBuf {
    crate::config::Config::dir().join("stats.json")
}

/// Persistent stats data saved to disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistentStats {
    requests: HashMap<String, u64>,
    endpoint_requests: HashMap<String, u64>,
    tokens: HashMap<String, PersistentTokenCounters>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistentTokenCounters {
    input: u64,
    output: u64,
    cache_read: u64,
}

/// Per-model token counters (atomic for lock-free reads)
struct TokenCounters {
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    cache_read_tokens: AtomicU64,
}

impl TokenCounters {
    fn new() -> Self {
        Self {
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            cache_read_tokens: AtomicU64::new(0),
        }
    }
}

/// A single token usage event with timestamp for time-series display
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TokenEvent {
    /// Seconds since server start
    pub elapsed_secs: u64,
    /// Model that generated this usage
    pub model: String,
    /// Input tokens consumed
    pub input_tokens: u32,
    /// Output tokens generated
    pub output_tokens: u32,
    /// Cache read tokens
    pub cache_read_tokens: u32,
}

/// Request/response statistics
pub struct Stats {
    /// Total requests by model
    requests: RwLock<HashMap<String, AtomicU64>>,
    /// Server start time
    start_time: Instant,
    /// Requests by endpoint
    endpoint_requests: RwLock<HashMap<String, AtomicU64>>,
    /// Request rate history (requests per second for last N seconds)
    rate_history: RwLock<RateHistory>,
    /// Per-model cumulative token counters
    token_counters: RwLock<HashMap<String, TokenCounters>>,
    /// Time-series of token events for graphing
    token_events: RwLock<VecDeque<TokenEvent>>,
}

/// Tracks requests per second over time
struct RateHistory {
    /// Ring buffer of request counts per second
    buckets: [u64; RATE_HISTORY_SIZE],
    /// Current bucket index
    current_idx: usize,
    /// Last update timestamp (second)
    last_second: u64,
    /// Requests in current second
    current_count: u64,
}

impl RateHistory {
    fn new() -> Self {
        Self {
            buckets: [0; RATE_HISTORY_SIZE],
            current_idx: 0,
            last_second: 0,
            current_count: 0,
        }
    }

    /// Record a request, updating buckets as needed
    fn record(&mut self, now_secs: u64) {
        if self.last_second == 0 {
            self.last_second = now_secs;
        }

        // If we're in a new second, rotate buckets
        while self.last_second < now_secs {
            // Store current count in bucket
            self.buckets[self.current_idx] = self.current_count;
            self.current_count = 0;
            self.current_idx = (self.current_idx + 1) % RATE_HISTORY_SIZE;
            self.last_second += 1;
        }

        self.current_count += 1;
    }

    /// Get the rate history as a vector (oldest to newest)
    fn get_history(&self, now_secs: u64) -> Vec<u64> {
        let mut result = Vec::with_capacity(RATE_HISTORY_SIZE);

        // Calculate how many seconds have passed since last update
        let elapsed = now_secs.saturating_sub(self.last_second) as usize;

        // Read buckets in order from oldest to newest
        for i in 0..RATE_HISTORY_SIZE {
            let idx = (self.current_idx + i + 1) % RATE_HISTORY_SIZE;
            // If this bucket is stale (beyond elapsed time), it's valid history
            // Otherwise it might be from a previous cycle
            if i < RATE_HISTORY_SIZE.saturating_sub(elapsed) {
                result.push(self.buckets[idx]);
            } else {
                result.push(0);
            }
        }

        // Add current count if we're in the same second
        if elapsed == 0
            && !result.is_empty()
            && let Some(last) = result.last_mut()
        {
            *last = self.current_count;
        }

        result
    }
}

impl Stats {
    fn new() -> Self {
        let stats = Self {
            requests: RwLock::new(HashMap::new()),
            start_time: Instant::now(),
            endpoint_requests: RwLock::new(HashMap::new()),
            rate_history: RwLock::new(RateHistory::new()),
            token_counters: RwLock::new(HashMap::new()),
            token_events: RwLock::new(VecDeque::with_capacity(MAX_TOKEN_EVENTS)),
        };
        stats.load_persistent();
        stats
    }

    /// Load persistent stats from disk, merging into current counters.
    fn load_persistent(&self) {
        let path = stats_path();
        if let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(persistent) = serde_json::from_str::<PersistentStats>(&data)
        {
            // Restore request counters
            let mut requests = self.requests.write();
            for (model, count) in persistent.requests {
                requests
                    .entry(model)
                    .or_insert_with(|| AtomicU64::new(0))
                    .fetch_add(count, Ordering::Relaxed);
            }
            drop(requests);

            // Restore endpoint counters
            let mut endpoints = self.endpoint_requests.write();
            for (endpoint, count) in persistent.endpoint_requests {
                endpoints
                    .entry(endpoint)
                    .or_insert_with(|| AtomicU64::new(0))
                    .fetch_add(count, Ordering::Relaxed);
            }
            drop(endpoints);

            // Restore token counters
            let mut counters = self.token_counters.write();
            for (model, tc) in persistent.tokens {
                let entry = counters
                    .entry(model)
                    .or_insert_with(TokenCounters::new);
                entry.input_tokens.fetch_add(tc.input, Ordering::Relaxed);
                entry.output_tokens.fetch_add(tc.output, Ordering::Relaxed);
                entry.cache_read_tokens.fetch_add(tc.cache_read, Ordering::Relaxed);
            }
        }
    }

    /// Save current cumulative stats to disk.
    pub fn save_persistent(&self) {
        let requests: HashMap<String, u64> = self
            .requests
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
            .collect();

        let endpoint_requests: HashMap<String, u64> = self
            .endpoint_requests
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
            .collect();

        let tokens: HashMap<String, PersistentTokenCounters> = self
            .token_counters
            .read()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    PersistentTokenCounters {
                        input: v.input_tokens.load(Ordering::Relaxed),
                        output: v.output_tokens.load(Ordering::Relaxed),
                        cache_read: v.cache_read_tokens.load(Ordering::Relaxed),
                    },
                )
            })
            .collect();

        let persistent = PersistentStats {
            requests,
            endpoint_requests,
            tokens,
        };

        let path = stats_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(&persistent) {
            let _ = std::fs::write(&path, data);
        }
    }

    /// Record a request
    pub fn record_request(&self, model: &str, endpoint: &str) {
        self.increment_map(&self.requests, model);
        self.increment_map(&self.endpoint_requests, endpoint);

        // Update rate history
        let now_secs = self.start_time.elapsed().as_secs();
        self.rate_history.write().record(now_secs);

        // Periodically save stats to disk (every 50 requests)
        let total = self.sum_map(&self.requests);
        if total.is_multiple_of(50) {
            self.save_persistent();
        }
    }

    /// Record token usage for a completed request
    pub fn record_token_usage(
        &self,
        model: &str,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
    ) {
        // Update per-model cumulative counters
        {
            let counters = self.token_counters.read();
            if let Some(c) = counters.get(model) {
                c.input_tokens
                    .fetch_add(input_tokens as u64, Ordering::Relaxed);
                c.output_tokens
                    .fetch_add(output_tokens as u64, Ordering::Relaxed);
                c.cache_read_tokens
                    .fetch_add(cache_read_tokens as u64, Ordering::Relaxed);
                // Fall through to record event
            } else {
                drop(counters);
                let mut counters = self.token_counters.write();
                let entry = counters
                    .entry(model.to_string())
                    .or_insert_with(TokenCounters::new);
                entry
                    .input_tokens
                    .fetch_add(input_tokens as u64, Ordering::Relaxed);
                entry
                    .output_tokens
                    .fetch_add(output_tokens as u64, Ordering::Relaxed);
                entry
                    .cache_read_tokens
                    .fetch_add(cache_read_tokens as u64, Ordering::Relaxed);
            }
        }

        // Record time-series event
        let elapsed_secs = self.start_time.elapsed().as_secs();
        let event = TokenEvent {
            elapsed_secs,
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
        };
        let mut events = self.token_events.write();
        if events.len() >= MAX_TOKEN_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// Get uptime
    pub fn uptime(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Get request rate history (requests per second for last 60 seconds)
    pub fn get_rate_history(&self) -> Vec<u64> {
        let now_secs = self.start_time.elapsed().as_secs();
        self.rate_history.read().get_history(now_secs)
    }

    /// Get recent token events for time-series display
    #[allow(dead_code)]
    pub fn get_token_events(&self) -> Vec<TokenEvent> {
        self.token_events.read().iter().cloned().collect()
    }

    /// Get summary statistics
    pub fn summary(&self) -> StatsSummary {
        StatsSummary {
            uptime: self.uptime(),
            total_requests: self.sum_map(&self.requests),
            models: self.get_model_stats(),
            endpoints: self.get_endpoint_stats(),
            rate_history: self.get_rate_history(),
            token_usage: self.get_token_usage(),
        }
    }

    fn get_model_stats(&self) -> Vec<ModelStats> {
        let requests = self.requests.read();
        let token_counters = self.token_counters.read();
        requests
            .iter()
            .map(|(model, count)| {
                let (input, output, cache_read) = if let Some(tc) = token_counters.get(model) {
                    (
                        tc.input_tokens.load(Ordering::Relaxed),
                        tc.output_tokens.load(Ordering::Relaxed),
                        tc.cache_read_tokens.load(Ordering::Relaxed),
                    )
                } else {
                    (0, 0, 0)
                };
                ModelStats {
                    model: model.clone(),
                    requests: count.load(Ordering::Relaxed),
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_tokens: cache_read,
                }
            })
            .collect()
    }

    fn get_endpoint_stats(&self) -> Vec<EndpointStats> {
        let endpoints = self.endpoint_requests.read();
        endpoints
            .iter()
            .map(|(endpoint, count)| EndpointStats {
                endpoint: endpoint.clone(),
                requests: count.load(Ordering::Relaxed),
            })
            .collect()
    }

    fn get_token_usage(&self) -> TokenUsageSummary {
        let counters = self.token_counters.read();
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut total_cache_read = 0u64;
        for tc in counters.values() {
            total_input += tc.input_tokens.load(Ordering::Relaxed);
            total_output += tc.output_tokens.load(Ordering::Relaxed);
            total_cache_read += tc.cache_read_tokens.load(Ordering::Relaxed);
        }
        TokenUsageSummary {
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read_tokens: total_cache_read,
        }
    }

    fn increment_map(&self, map: &RwLock<HashMap<String, AtomicU64>>, key: &str) {
        {
            let read = map.read();
            if let Some(counter) = read.get(key) {
                counter.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let mut write = map.write();
        write
            .entry(key.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    fn sum_map(&self, map: &RwLock<HashMap<String, AtomicU64>>) -> u64 {
        let read = map.read();
        read.values().map(|v| v.load(Ordering::Relaxed)).sum()
    }
}

#[derive(Debug, Clone)]
pub struct StatsSummary {
    pub uptime: Duration,
    pub total_requests: u64,
    pub models: Vec<ModelStats>,
    pub endpoints: Vec<EndpointStats>,
    pub rate_history: Vec<u64>,
    pub token_usage: TokenUsageSummary,
}

#[derive(Debug, Clone)]
pub struct ModelStats {
    pub model: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct EndpointStats {
    pub endpoint: String,
    pub requests: u64,
}

#[derive(Debug, Clone)]
pub struct TokenUsageSummary {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
}

impl StatsSummary {
    /// Convert to JSON
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "uptime_seconds": self.uptime.as_secs(),
            "total_requests": self.total_requests,
            "models": self.models.iter().map(|m| serde_json::json!({
                "model": m.model,
                "requests": m.requests,
                "input_tokens": m.input_tokens,
                "output_tokens": m.output_tokens,
                "cache_read_tokens": m.cache_read_tokens,
            })).collect::<Vec<_>>(),
            "endpoints": self.endpoints.iter().map(|e| serde_json::json!({
                "endpoint": e.endpoint,
                "requests": e.requests,
            })).collect::<Vec<_>>(),
            "rate_history": self.rate_history,
            "token_usage": {
                "total_input_tokens": self.token_usage.total_input_tokens,
                "total_output_tokens": self.token_usage.total_output_tokens,
                "total_cache_read_tokens": self.token_usage.total_cache_read_tokens,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_record_request() {
        let stats = Stats::new();
        stats.record_request("claude-sonnet-4-5", "/v1/messages");
        stats.record_request("claude-sonnet-4-5", "/v1/messages");
        stats.record_request("claude-opus-4-5", "/v1/chat/completions");

        let summary = stats.summary();
        assert_eq!(summary.total_requests, 3);
        assert_eq!(summary.models.len(), 2);
        assert_eq!(summary.endpoints.len(), 2);
    }

    #[test]
    fn test_stats_uptime() {
        let stats = Stats::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(stats.uptime().as_millis() >= 10);
    }

    #[test]
    fn test_stats_to_json() {
        let stats = Stats::new();
        stats.record_request("test-model", "/v1/messages");

        let summary = stats.summary();
        let json = summary.to_json();

        assert!(json["uptime_seconds"].as_u64().is_some());
        assert_eq!(json["total_requests"].as_u64(), Some(1));
    }

    #[test]
    fn test_stats_token_usage() {
        let stats = Stats::new();
        stats.record_request("claude-sonnet-4-5", "/v1/messages");
        stats.record_token_usage("claude-sonnet-4-5", 100, 200, 50);
        stats.record_token_usage("claude-sonnet-4-5", 150, 300, 0);
        stats.record_token_usage("gemini-3-flash", 80, 160, 0);

        let summary = stats.summary();

        // Check per-model tokens
        let sonnet = summary
            .models
            .iter()
            .find(|m| m.model == "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(sonnet.input_tokens, 250);
        assert_eq!(sonnet.output_tokens, 500);
        assert_eq!(sonnet.cache_read_tokens, 50);

        // Check totals
        assert_eq!(summary.token_usage.total_input_tokens, 330);
        assert_eq!(summary.token_usage.total_output_tokens, 660);
        assert_eq!(summary.token_usage.total_cache_read_tokens, 50);

        // Check events
        let events = stats.get_token_events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].model, "claude-sonnet-4-5");
        assert_eq!(events[2].model, "gemini-3-flash");
    }

    #[test]
    fn test_stats_token_json() {
        let stats = Stats::new();
        stats.record_request("test-model", "/v1/messages");
        stats.record_token_usage("test-model", 100, 200, 0);

        let json = stats.summary().to_json();
        let token_usage = &json["token_usage"];
        assert_eq!(token_usage["total_input_tokens"].as_u64(), Some(100));
        assert_eq!(token_usage["total_output_tokens"].as_u64(), Some(200));

        let model = &json["models"][0];
        assert_eq!(model["input_tokens"].as_u64(), Some(100));
        assert_eq!(model["output_tokens"].as_u64(), Some(200));
    }
}
