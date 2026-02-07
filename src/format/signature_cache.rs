use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Minimum valid thinking signature length
pub const MIN_SIGNATURE_LENGTH: usize = 50;

/// Cache TTL for signatures (2 hours)
const SIGNATURE_CACHE_TTL: Duration = Duration::from_secs(2 * 60 * 60);

/// Skip signature validator sentinel value for Gemini
pub const GEMINI_SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// Model family for signature tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    Gemini,
}

impl ModelFamily {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct CacheEntry<T> {
    value: T,
    timestamp: Instant,
}

impl<T> CacheEntry<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            timestamp: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.timestamp.elapsed() > SIGNATURE_CACHE_TTL
    }
}

/// Global signature cache for tool_use IDs -> thoughtSignature
static TOOL_SIGNATURE_CACHE: LazyLock<RwLock<HashMap<String, CacheEntry<String>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Global thinking signature cache: signature -> model family
static THINKING_SIGNATURE_CACHE: LazyLock<RwLock<HashMap<String, CacheEntry<ModelFamily>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Cache a signature for a tool_use_id
///
/// When Gemini returns a functionCall with a thoughtSignature, we cache it
/// so that when Claude Code sends back a tool_result (which may have stripped
/// the signature), we can restore it for the next request.
pub fn cache_tool_signature(tool_use_id: &str, signature: &str) {
    if tool_use_id.is_empty() || signature.is_empty() {
        return;
    }
    if signature.len() < MIN_SIGNATURE_LENGTH {
        return;
    }

    let mut cache = TOOL_SIGNATURE_CACHE.write();
    cache.insert(
        tool_use_id.to_string(),
        CacheEntry::new(signature.to_string()),
    );
}

/// Get a cached signature for a tool_use_id
///
/// Returns None if not found or expired
pub fn get_cached_tool_signature(tool_use_id: &str) -> Option<String> {
    if tool_use_id.is_empty() {
        return None;
    }

    let mut cache = TOOL_SIGNATURE_CACHE.write();

    if let Some(entry) = cache.get(tool_use_id) {
        if entry.is_expired() {
            cache.remove(tool_use_id);
            return None;
        }
        return Some(entry.value.clone());
    }

    None
}

/// Cache a thinking signature with its model family
///
/// This allows us to track which model family generated a particular signature,
/// enabling cross-model compatibility checks.
pub fn cache_thinking_signature(signature: &str, family: ModelFamily) {
    if signature.is_empty() || signature.len() < MIN_SIGNATURE_LENGTH {
        return;
    }

    let mut cache = THINKING_SIGNATURE_CACHE.write();
    cache.insert(signature.to_string(), CacheEntry::new(family));
}

/// Get the cached model family for a thinking signature
///
/// Returns None if not found or expired
pub fn get_cached_signature_family(signature: &str) -> Option<ModelFamily> {
    if signature.is_empty() {
        return None;
    }

    let mut cache = THINKING_SIGNATURE_CACHE.write();

    if let Some(entry) = cache.get(signature) {
        if entry.is_expired() {
            cache.remove(signature);
            return None;
        }
        return Some(entry.value);
    }

    None
}

/// Check if a signature is compatible with a target model family
///
/// For Gemini targets: only accept signatures from Gemini (strict validation)
/// For Claude targets: accept all signatures (Claude validates its own)
pub fn is_signature_compatible(signature: &str, target_family: ModelFamily) -> bool {
    // For Claude, we're lenient - let Claude validate its own signatures
    if target_family == ModelFamily::Claude {
        return true;
    }

    // For Gemini, check if we know the source family
    match get_cached_signature_family(signature) {
        Some(source_family) => source_family == target_family,
        // Unknown signature origin - for Gemini, reject (safe default)
        None => false,
    }
}

/// Clear all signature caches (for testing)
#[cfg(test)]
pub fn clear_caches() {
    TOOL_SIGNATURE_CACHE.write().clear();
    THINKING_SIGNATURE_CACHE.write().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_signature_cache() {
        clear_caches();

        let tool_id = "toolu_12345";
        let signature = "a".repeat(MIN_SIGNATURE_LENGTH);

        // Should be None initially
        assert!(get_cached_tool_signature(tool_id).is_none());

        // Cache it
        cache_tool_signature(tool_id, &signature);

        // Should be found now
        assert_eq!(get_cached_tool_signature(tool_id), Some(signature.clone()));
    }

    #[test]
    fn test_tool_signature_too_short() {
        clear_caches();

        let tool_id = "toolu_short";
        let short_signature = "a".repeat(MIN_SIGNATURE_LENGTH - 1);

        // Should not cache short signatures
        cache_tool_signature(tool_id, &short_signature);
        assert!(get_cached_tool_signature(tool_id).is_none());
    }

    #[test]
    fn test_thinking_signature_cache() {
        clear_caches();

        let signature = "b".repeat(MIN_SIGNATURE_LENGTH);

        // Should be None initially
        assert!(get_cached_signature_family(&signature).is_none());

        // Cache it with Gemini family
        cache_thinking_signature(&signature, ModelFamily::Gemini);

        // Should be found now
        assert_eq!(
            get_cached_signature_family(&signature),
            Some(ModelFamily::Gemini)
        );
    }

    #[test]
    fn test_signature_compatibility_claude() {
        clear_caches();

        let signature = "c".repeat(MIN_SIGNATURE_LENGTH);

        // Claude is lenient - accepts any signature
        assert!(is_signature_compatible(&signature, ModelFamily::Claude));

        // Even Gemini-sourced signatures are ok for Claude
        cache_thinking_signature(&signature, ModelFamily::Gemini);
        assert!(is_signature_compatible(&signature, ModelFamily::Claude));
    }

    #[test]
    fn test_signature_compatibility_gemini() {
        clear_caches();

        let signature = "d".repeat(MIN_SIGNATURE_LENGTH);

        // Gemini is strict - rejects unknown signatures
        assert!(!is_signature_compatible(&signature, ModelFamily::Gemini));

        // Accept Gemini-sourced signatures
        cache_thinking_signature(&signature, ModelFamily::Gemini);
        assert!(is_signature_compatible(&signature, ModelFamily::Gemini));

        // Reject Claude-sourced signatures for Gemini
        let claude_sig = "e".repeat(MIN_SIGNATURE_LENGTH);
        cache_thinking_signature(&claude_sig, ModelFamily::Claude);
        assert!(!is_signature_compatible(&claude_sig, ModelFamily::Gemini));
    }

    #[test]
    fn test_model_family_from_str() {
        assert_eq!(ModelFamily::from_str("claude"), Some(ModelFamily::Claude));
        assert_eq!(ModelFamily::from_str("Claude"), Some(ModelFamily::Claude));
        assert_eq!(ModelFamily::from_str("gemini"), Some(ModelFamily::Gemini));
        assert_eq!(ModelFamily::from_str("GEMINI"), Some(ModelFamily::Gemini));
        assert_eq!(ModelFamily::from_str("unknown"), None);
    }
}
