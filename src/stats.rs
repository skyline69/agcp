use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Number of seconds to track for the request rate graph
const RATE_HISTORY_SIZE: usize = 60;

/// Global stats instance
static STATS: std::sync::LazyLock<Stats> = std::sync::LazyLock::new(Stats::new);

/// Get the global stats instance
pub fn get_stats() -> &'static Stats {
    &STATS
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
        Self {
            requests: RwLock::new(HashMap::new()),
            start_time: Instant::now(),
            endpoint_requests: RwLock::new(HashMap::new()),
            rate_history: RwLock::new(RateHistory::new()),
        }
    }

    /// Record a request
    pub fn record_request(&self, model: &str, endpoint: &str) {
        self.increment_map(&self.requests, model);
        self.increment_map(&self.endpoint_requests, endpoint);

        // Update rate history
        let now_secs = self.start_time.elapsed().as_secs();
        self.rate_history.write().record(now_secs);
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

    /// Get summary statistics
    pub fn summary(&self) -> StatsSummary {
        StatsSummary {
            uptime: self.uptime(),
            total_requests: self.sum_map(&self.requests),
            models: self.get_model_stats(),
            endpoints: self.get_endpoint_stats(),
            rate_history: self.get_rate_history(),
        }
    }

    fn get_model_stats(&self) -> Vec<ModelStats> {
        let requests = self.requests.read();
        requests
            .iter()
            .map(|(model, count)| ModelStats {
                model: model.clone(),
                requests: count.load(Ordering::Relaxed),
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
}

#[derive(Debug, Clone)]
pub struct ModelStats {
    pub model: String,
    pub requests: u64,
}

#[derive(Debug, Clone)]
pub struct EndpointStats {
    pub endpoint: String,
    pub requests: u64,
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
            })).collect::<Vec<_>>(),
            "endpoints": self.endpoints.iter().map(|e| serde_json::json!({
                "endpoint": e.endpoint,
                "requests": e.requests,
            })).collect::<Vec<_>>(),
            "rate_history": self.rate_history,
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
}
