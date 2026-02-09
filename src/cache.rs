//! Response cache with LRU eviction and TTL expiration.

use hyper::body::Bytes;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// A single cache entry with TTL tracking.
struct CacheEntry {
    response: Bytes,
    created_at: Instant,
    ttl: Duration,
}

impl CacheEntry {
    /// Check if this entry has expired.
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= self.ttl
    }
}

/// Statistics about cache usage.
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub enabled: bool,
    pub entries: usize,
    pub max_entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
}

/// Response cache with LRU eviction and TTL expiration.
pub struct ResponseCache {
    entries: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
    max_entries: usize,
    default_ttl: Duration,
    enabled: bool,
    hits: u64,
    misses: u64,
}

impl ResponseCache {
    /// Create a new response cache.
    ///
    /// # Arguments
    /// * `enabled` - Whether caching is enabled
    /// * `ttl_seconds` - Default TTL for cache entries in seconds
    /// * `max_entries` - Maximum number of entries before LRU eviction
    pub fn new(enabled: bool, ttl_seconds: u64, max_entries: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(max_entries),
            order: VecDeque::with_capacity(max_entries),
            max_entries,
            default_ttl: Duration::from_secs(ttl_seconds),
            enabled,
            hits: 0,
            misses: 0,
        }
    }

    /// Generate a cache key from request parameters using SHA-256.
    ///
    /// The key is a deterministic hash of the model, messages, system prompt,
    /// tools, and temperature. Returns a hex-encoded string (64 chars).
    pub fn make_key(
        model: &str,
        messages_json: &str,
        system_json: Option<&str>,
        tools_json: Option<&str>,
        temperature: Option<f32>,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(model.as_bytes());
        hasher.update(b"|");
        hasher.update(messages_json.as_bytes());
        hasher.update(b"|");
        if let Some(system) = system_json {
            hasher.update(system.as_bytes());
        }
        hasher.update(b"|");
        if let Some(tools) = tools_json {
            hasher.update(tools.as_bytes());
        }
        hasher.update(b"|");
        if let Some(temp) = temperature {
            hasher.update(temp.to_le_bytes());
        }
        let result = hasher.finalize();
        // Use a pre-allocated string and write hex directly (avoids per-byte format!)
        let mut hex = String::with_capacity(64);
        for b in result.iter() {
            use std::fmt::Write;
            let _ = write!(hex, "{:02x}", b);
        }
        hex
    }

    /// Get a cached response by key.
    ///
    /// Returns `Some(response)` if found and not expired, `None` otherwise.
    /// Updates LRU order and tracks hits/misses.
    /// The returned `Bytes` is cheaply cloned (reference-counted).
    pub fn get(&mut self, key: &str) -> Option<Bytes> {
        if !self.enabled {
            self.misses += 1;
            return None;
        }

        // Check if entry exists and is not expired
        if let Some(entry) = self.entries.get(key) {
            if entry.is_expired() {
                // Remove expired entry
                self.entries.remove(key);
                self.order.retain(|k| k != key);
                self.misses += 1;
                return None;
            }

            // Hit - update LRU order (move to back = most recently used)
            self.order.retain(|k| k != key);
            self.order.push_back(key.to_string());
            self.hits += 1;

            return Some(entry.response.clone());
        }

        self.misses += 1;
        None
    }

    /// Store a response in the cache.
    ///
    /// If the cache is at capacity, evicts the least recently used entry.
    pub fn put(&mut self, key: String, response: Vec<u8>) {
        if !self.enabled {
            return;
        }

        let response = Bytes::from(response);

        // If key already exists, update it and move to back of LRU
        if self.entries.contains_key(&key) {
            self.entries.insert(
                key.clone(),
                CacheEntry {
                    response,
                    created_at: Instant::now(),
                    ttl: self.default_ttl,
                },
            );
            self.order.retain(|k| k != &key);
            self.order.push_back(key);
            return;
        }

        // Evict LRU entries if at capacity
        while self.entries.len() >= self.max_entries {
            if let Some(oldest_key) = self.order.pop_front() {
                self.entries.remove(&oldest_key);
            } else {
                break;
            }
        }

        // Insert new entry
        self.entries.insert(
            key.clone(),
            CacheEntry {
                response,
                created_at: Instant::now(),
                ttl: self.default_ttl,
            },
        );
        self.order.push_back(key);
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        let total = self.hits + self.misses;
        let hit_rate = if total > 0 {
            self.hits as f64 / total as f64
        } else {
            0.0
        };

        CacheStats {
            enabled: self.enabled,
            entries: self.entries.len(),
            max_entries: self.max_entries,
            hits: self.hits,
            misses: self.misses,
            hit_rate,
        }
    }

    /// Clear all cache entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_basic_operations() {
        let mut cache = ResponseCache::new(true, 3600, 100);

        let key = ResponseCache::make_key("claude-3", r#"[{"role":"user"}]"#, None, None, None);
        let response = b"test response".to_vec();

        // Initially empty
        assert!(cache.get(&key).is_none());

        // Put and get
        cache.put(key.clone(), response.clone());
        assert_eq!(cache.get(&key).as_deref(), Some(response.as_slice()));

        // Still there on second get
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn test_cache_disabled() {
        let mut cache = ResponseCache::new(false, 3600, 100);

        let key = "test_key".to_string();
        let response = b"test response".to_vec();

        // Put should do nothing when disabled
        cache.put(key.clone(), response.clone());

        // Get should return None when disabled
        assert!(cache.get(&key).is_none());

        // Verify nothing was stored
        assert_eq!(cache.entries.len(), 0);
        assert_eq!(cache.order.len(), 0);
    }

    #[test]
    fn test_cache_lru_eviction() {
        let mut cache = ResponseCache::new(true, 3600, 3);

        // Fill cache to capacity
        cache.put("key1".to_string(), b"response1".to_vec());
        cache.put("key2".to_string(), b"response2".to_vec());
        cache.put("key3".to_string(), b"response3".to_vec());

        assert_eq!(cache.entries.len(), 3);

        // Access key1 to make it recently used
        assert!(cache.get("key1").is_some());

        // Add a new entry - should evict key2 (least recently used)
        cache.put("key4".to_string(), b"response4".to_vec());

        assert_eq!(cache.entries.len(), 3);
        assert!(cache.get("key1").is_some()); // Still there (was accessed)
        assert!(cache.get("key2").is_none()); // Evicted
        assert!(cache.get("key3").is_some()); // Still there
        assert!(cache.get("key4").is_some()); // Just added
    }

    #[test]
    fn test_cache_key_generation() {
        // Same inputs should produce same key
        let key1 = ResponseCache::make_key(
            "claude-3",
            r#"[{"role":"user","content":"hello"}]"#,
            Some("system prompt"),
            None,
            Some(0.7),
        );
        let key2 = ResponseCache::make_key(
            "claude-3",
            r#"[{"role":"user","content":"hello"}]"#,
            Some("system prompt"),
            None,
            Some(0.7),
        );
        assert_eq!(key1, key2);

        // Different model should produce different key
        let key3 = ResponseCache::make_key(
            "claude-4",
            r#"[{"role":"user","content":"hello"}]"#,
            Some("system prompt"),
            None,
            Some(0.7),
        );
        assert_ne!(key1, key3);

        // Different messages should produce different key
        let key4 = ResponseCache::make_key(
            "claude-3",
            r#"[{"role":"user","content":"goodbye"}]"#,
            Some("system prompt"),
            None,
            Some(0.7),
        );
        assert_ne!(key1, key4);

        // Different temperature should produce different key
        let key5 = ResponseCache::make_key(
            "claude-3",
            r#"[{"role":"user","content":"hello"}]"#,
            Some("system prompt"),
            None,
            Some(0.9),
        );
        assert_ne!(key1, key5);

        // Different system prompt should produce different key
        let key6 = ResponseCache::make_key(
            "claude-3",
            r#"[{"role":"user","content":"hello"}]"#,
            Some("different system"),
            None,
            Some(0.7),
        );
        assert_ne!(key1, key6);

        // Key should be valid hex (64 chars for SHA-256)
        assert_eq!(key1.len(), 64);
        assert!(key1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_cache_stats() {
        let mut cache = ResponseCache::new(true, 3600, 100);

        // Initial stats
        let stats = cache.stats();
        assert!(stats.enabled);
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.max_entries, 100);
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.hit_rate, 0.0);

        // Add an entry and miss
        cache.put("key1".to_string(), b"response1".to_vec());
        cache.get("key2"); // Miss

        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hit_rate, 0.0);

        // Hit
        cache.get("key1"); // Hit

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hit_rate, 0.5);

        // Another hit
        cache.get("key1"); // Hit

        let stats = cache.stats();
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate - 0.666666).abs() < 0.001);
    }
}
