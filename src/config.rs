use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::LazyLock;

/// Error type for configuration loading
#[derive(Debug)]
pub enum ConfigError {
    ReadError {
        path: PathBuf,
        source: std::io::Error,
    },
    ParseError {
        path: PathBuf,
        source: toml::de::Error,
    },
    InvalidValue {
        path: PathBuf,
        field: String,
        value: String,
        valid_values: Vec<String>,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::ReadError { path, source } => {
                write!(
                    f,
                    "Failed to read config file {}: {}",
                    path.display(),
                    source
                )
            }
            ConfigError::ParseError { path, source } => {
                write!(f, "Invalid TOML syntax in {}: {}", path.display(), source)
            }
            ConfigError::InvalidValue {
                path,
                field,
                value,
                valid_values,
            } => {
                write!(
                    f,
                    "Invalid value '{}' for '{}' in {}\n  Valid values: {}",
                    value,
                    field,
                    path.display(),
                    valid_values.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::ReadError { source, .. } => Some(source),
            ConfigError::ParseError { source, .. } => Some(source),
            ConfigError::InvalidValue { .. } => None,
        }
    }
}

/// Global config instance (uses default if load fails at static init)
static GLOBAL_CONFIG: LazyLock<RwLock<Config>> =
    LazyLock::new(|| RwLock::new(Config::load().unwrap_or_default()));

/// Get a reference to the global config
pub fn get_config() -> Config {
    GLOBAL_CONFIG.read().clone()
}

/// Initialize global config with overrides
pub fn init_config(config: Config) {
    *GLOBAL_CONFIG.write() = config;
}

/// AGCP configuration loaded from `~/.config/agcp/config.toml`.
///
/// All fields have sensible defaults and can be overridden via CLI flags.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub accounts: AccountsConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub cloudcode: CloudCodeConfig,
    #[serde(default)]
    pub mappings: MappingsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    /// Optional API key for authenticating requests to /v1/* endpoints
    #[serde(default)]
    pub api_key: Option<String>,
    /// Request timeout in seconds (default: 300 = 5 minutes)
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoggingConfig {
    #[serde(default)]
    pub debug: bool,
    /// Log full request/response bodies for debugging
    #[serde(default)]
    pub log_requests: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsConfig {
    /// Selection strategy: "sticky", "roundrobin", or "hybrid"
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Quota threshold (0.0-1.0) - accounts below this are deprioritized
    #[serde(default = "default_quota_threshold")]
    pub quota_threshold: f64,
    /// Enable model fallback on quota exhaustion
    #[serde(default)]
    pub fallback: bool,
}

fn default_strategy() -> String {
    "hybrid".to_string()
}

fn default_quota_threshold() -> f64 {
    0.1
}

impl Default for AccountsConfig {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            quota_threshold: default_quota_threshold(),
            fallback: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Enable response caching for non-streaming requests
    #[serde(default = "default_cache_enabled")]
    pub enabled: bool,
    /// Cache TTL in seconds (default: 300 = 5 minutes)
    #[serde(default = "default_cache_ttl")]
    pub ttl_seconds: u64,
    /// Maximum number of cached responses (default: 100)
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: usize,
}

fn default_cache_enabled() -> bool {
    true
}

fn default_cache_ttl() -> u64 {
    300
}

fn default_cache_max_entries() -> usize {
    100
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_cache_enabled(),
            ttl_seconds: default_cache_ttl(),
            max_entries: default_cache_max_entries(),
        }
    }
}

/// Configuration for the Google Cloud Code API client.
///
/// Example in `config.toml`:
/// ```toml
/// [cloudcode]
/// timeout_secs = 180
/// max_retries = 3
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudCodeConfig {
    /// API timeout in seconds (default: 120)
    #[serde(default = "default_api_timeout")]
    pub timeout_secs: u64,
    /// Maximum retry attempts for failed requests (default: 5)
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Maximum concurrent requests to Cloud Code API (default: 1)
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    /// Minimum interval between requests in milliseconds (default: 500)
    #[serde(default = "default_min_request_interval")]
    pub min_request_interval_ms: u64,
}

fn default_api_timeout() -> u64 {
    120
}

fn default_max_retries() -> u32 {
    5
}

fn default_max_concurrent() -> usize {
    1
}

fn default_min_request_interval() -> u64 {
    500
}

impl Default for CloudCodeConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_api_timeout(),
            max_retries: default_max_retries(),
            max_concurrent_requests: default_max_concurrent(),
            min_request_interval_ms: default_min_request_interval(),
        }
    }
}

/// A single model mapping rule: glob pattern -> target model.
///
/// Example in `config.toml`:
/// ```toml
/// [[mappings.rules]]
/// from = "claude-3-haiku-*"
/// to = "gemini-3-flash"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MappingRule {
    /// Glob pattern to match incoming model names (e.g. "claude-3-haiku-*", "gpt-4*")
    pub from: String,
    /// Target model to resolve to (e.g. "gemini-3-flash")
    pub to: String,
}

/// Configuration for model name mappings and presets.
///
/// Example in `config.toml`:
/// ```toml
/// [mappings]
/// preset = "balanced"
/// background_task_model = "gemini-3-flash"
///
/// [[mappings.rules]]
/// from = "gpt-4*"
/// to = "gemini-3-pro-high"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappingsConfig {
    /// Active preset: "balanced", "performance", "cost", "custom", or "none"
    #[serde(default = "default_preset")]
    pub preset: String,
    /// Model used for background tasks (title generation, summaries, etc.)
    #[serde(default = "default_background_model")]
    pub background_task_model: String,
    /// Custom mapping rules (glob pattern -> target model). First match wins.
    #[serde(default)]
    pub rules: Vec<MappingRule>,
}

fn default_preset() -> String {
    "balanced".to_string()
}

fn default_background_model() -> String {
    "gemini-3-flash".to_string()
}

impl Default for MappingsConfig {
    fn default() -> Self {
        Self {
            preset: default_preset(),
            background_task_model: default_background_model(),
            rules: Vec::new(),
        }
    }
}

fn default_port() -> u16 {
    8080
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_request_timeout() -> u64 {
    300
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            api_key: None,
            request_timeout_secs: default_request_timeout(),
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("agcp")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::path();
        if path.exists() {
            let content = std::fs::read_to_string(&path).map_err(|e| ConfigError::ReadError {
                path: path.clone(),
                source: e,
            })?;
            let config: Config = toml::from_str(&content).map_err(|e| ConfigError::ParseError {
                path: path.clone(),
                source: e,
            })?;

            // Validate strategy
            let valid_strategies = vec![
                "sticky".to_string(),
                "roundrobin".to_string(),
                "hybrid".to_string(),
            ];
            let strategy_lower = config.accounts.strategy.to_lowercase();
            if !valid_strategies.contains(&strategy_lower)
                && !["round-robin", "rr", "smart"].contains(&strategy_lower.as_str())
            {
                return Err(ConfigError::InvalidValue {
                    path,
                    field: "accounts.strategy".to_string(),
                    value: config.accounts.strategy,
                    valid_values: valid_strategies,
                });
            }

            // Validate quota_threshold is in range
            if !(0.0..=1.0).contains(&config.accounts.quota_threshold) {
                return Err(ConfigError::InvalidValue {
                    path,
                    field: "accounts.quota_threshold".to_string(),
                    value: config.accounts.quota_threshold.to_string(),
                    valid_values: vec!["0.0 to 1.0".to_string()],
                });
            }

            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to the config file
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::path();

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::ReadError {
                path: path.clone(),
                source: e,
            })?;
        }

        let content = toml::to_string_pretty(self).map_err(|e| ConfigError::InvalidValue {
            path: path.clone(),
            field: "serialization".to_string(),
            value: e.to_string(),
            valid_values: vec![],
        })?;

        std::fs::write(&path, content).map_err(|e| ConfigError::ReadError {
            path: path.clone(),
            source: e,
        })?;

        Ok(())
    }

    /// Convenience accessors for backward compatibility
    pub fn port(&self) -> u16 {
        self.server.port
    }

    pub fn host(&self) -> &str {
        &self.server.host
    }

    pub fn with_overrides(mut self, port: Option<u16>, host: Option<String>, debug: bool) -> Self {
        if let Some(p) = port {
            self.server.port = p;
        }
        if let Some(h) = host {
            self.server.host = h;
        }
        if debug {
            self.logging.debug = true;
        }
        // Check for API_KEY environment variable
        if let Ok(api_key) = std::env::var("API_KEY") {
            self.server.api_key = Some(api_key);
        }
        self
    }
}

pub mod dirs {
    use std::path::PathBuf;

    pub fn config_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        }
        #[cfg(target_os = "linux")]
        {
            std::env::var("XDG_CONFIG_HOME")
                .ok()
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var("HOME")
                        .ok()
                        .map(|h| PathBuf::from(h).join(".config"))
                })
        }
        #[cfg(target_os = "windows")]
        {
            std::env::var("APPDATA").ok().map(PathBuf::from)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.host, "127.0.0.1");
        assert!(!config.logging.debug);
        assert!(!config.logging.log_requests);
    }

    #[test]
    fn test_config_with_overrides() {
        let config = Config::default();
        let config = config.with_overrides(Some(3000), Some("0.0.0.0".to_string()), true);

        assert_eq!(config.server.port, 3000);
        assert_eq!(config.server.host, "0.0.0.0");
        assert!(config.logging.debug);
    }

    #[test]
    fn test_config_partial_overrides() {
        let config = Config::default();
        let config = config.with_overrides(None, None, false);

        // Should keep defaults
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.host, "127.0.0.1");
        assert!(!config.logging.debug);
    }

    #[test]
    fn test_config_accessors() {
        let config = Config::default();
        assert_eq!(config.port(), 8080);
        assert_eq!(config.host(), "127.0.0.1");
    }

    #[test]
    fn test_config_path() {
        let path = Config::path();
        assert!(path.to_string_lossy().contains("agcp"));
        assert!(path.to_string_lossy().ends_with("config.toml"));
    }

    #[test]
    fn test_config_dir() {
        let dir = Config::dir();
        assert!(dir.to_string_lossy().contains("agcp"));
    }

    #[test]
    fn test_config_error_display() {
        let parse_error = toml::from_str::<Config>("invalid toml [").unwrap_err();
        let error = ConfigError::ParseError {
            path: PathBuf::from("/test/config.toml"),
            source: parse_error,
        };
        let msg = error.to_string();
        assert!(msg.contains("Invalid TOML syntax"));
        assert!(msg.contains("/test/config.toml"));
    }
}
