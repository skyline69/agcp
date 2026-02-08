//! Configuration field definitions for the interactive config editor.

use crate::config::Config;

/// Type of a configuration field
#[derive(Debug, Clone)]
pub enum FieldType {
    /// Free-form text (numbers, strings)
    Text,
    /// Boolean toggle
    Bool,
    /// Enumeration with valid values
    Enum(Vec<&'static str>),
    /// Bounded float value
    Float { min: f64, max: f64 },
}

/// A single editable configuration field
#[derive(Debug, Clone)]
pub struct ConfigField {
    pub section: &'static str,
    pub key: &'static str,
    pub field_type: FieldType,
    pub value: String,
    pub original: String,
    pub description: &'static str,
}

impl ConfigField {
    pub fn new(
        section: &'static str,
        key: &'static str,
        field_type: FieldType,
        value: String,
        description: &'static str,
    ) -> Self {
        Self {
            section,
            key,
            field_type,
            original: value.clone(),
            value,
            description,
        }
    }

    /// Check if value has been modified
    pub fn is_modified(&self) -> bool {
        self.value != self.original
    }

    /// Check if this field expects numeric input only
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.key,
            "port"
                | "request_timeout_secs"
                | "ttl_seconds"
                | "timeout_secs"
                | "max_retries"
                | "max_entries"
                | "min_request_interval_ms"
                | "max_concurrent_requests"
        ) || matches!(self.field_type, FieldType::Float { .. })
    }

    /// Validate the current value
    pub fn validate(&self) -> Result<(), String> {
        match &self.field_type {
            FieldType::Text => {
                // Text fields that are numbers need parsing
                if self.key == "port" {
                    let port: u16 = self.value.parse().map_err(|_| "Must be a number 1-65535")?;
                    if port == 0 {
                        return Err("Port must be 1-65535".to_string());
                    }
                } else if matches!(
                    self.key,
                    "request_timeout_secs"
                        | "ttl_seconds"
                        | "timeout_secs"
                        | "max_retries"
                        | "max_entries"
                        | "min_request_interval_ms"
                        | "max_concurrent_requests"
                ) {
                    self.value
                        .parse::<u64>()
                        .map_err(|_| "Must be a positive number")?;
                }
                Ok(())
            }
            FieldType::Bool => {
                if !matches!(self.value.as_str(), "true" | "false") {
                    Err("Must be true or false".to_string())
                } else {
                    Ok(())
                }
            }
            FieldType::Enum(valid) => {
                if valid.contains(&self.value.as_str()) {
                    Ok(())
                } else {
                    Err(format!("Must be one of: {}", valid.join(", ")))
                }
            }
            FieldType::Float { min, max } => {
                let val: f64 = self
                    .value
                    .parse()
                    .map_err(|_| format!("Must be a number between {} and {}", min, max))?;
                if val < *min || val > *max {
                    Err(format!("Must be between {} and {}", min, max))
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// Build the list of editable config fields from current config
pub fn build_config_fields(config: &Config) -> Vec<ConfigField> {
    vec![
        // Server section
        ConfigField::new(
            "server",
            "port",
            FieldType::Text,
            config.server.port.to_string(),
            "TCP port the proxy listens on (1-65535)",
        ),
        ConfigField::new(
            "server",
            "host",
            FieldType::Text,
            config.server.host.clone(),
            "Bind address for the proxy (e.g. 127.0.0.1 or 0.0.0.0 for all interfaces)",
        ),
        ConfigField::new(
            "server",
            "request_timeout_secs",
            FieldType::Text,
            config.server.request_timeout_secs.to_string(),
            "Maximum time in seconds to wait for a response before timing out",
        ),
        // Logging section
        ConfigField::new(
            "logging",
            "debug",
            FieldType::Bool,
            config.logging.debug.to_string(),
            "Enable verbose debug logging (includes request/response details)",
        ),
        ConfigField::new(
            "logging",
            "log_requests",
            FieldType::Bool,
            config.logging.log_requests.to_string(),
            "Log each API request with model, status, and duration",
        ),
        // Accounts section
        ConfigField::new(
            "accounts",
            "strategy",
            FieldType::Enum(vec!["sticky", "roundrobin", "hybrid"]),
            config.accounts.strategy.clone(),
            "Account selection strategy: sticky (stay until rate-limited), roundrobin (rotate each request), hybrid (smart selection)",
        ),
        ConfigField::new(
            "accounts",
            "quota_threshold",
            FieldType::Float { min: 0.0, max: 1.0 },
            config.accounts.quota_threshold.to_string(),
            "Switch accounts when quota remaining drops below this fraction (0.0-1.0)",
        ),
        ConfigField::new(
            "accounts",
            "fallback",
            FieldType::Bool,
            config.accounts.fallback.to_string(),
            "Try alternate model endpoints when the primary returns capacity errors",
        ),
        // Cache section
        ConfigField::new(
            "cache",
            "enabled",
            FieldType::Bool,
            config.cache.enabled.to_string(),
            "Enable response caching for identical requests (reduces API usage)",
        ),
        ConfigField::new(
            "cache",
            "ttl_seconds",
            FieldType::Text,
            config.cache.ttl_seconds.to_string(),
            "How long cached responses remain valid, in seconds",
        ),
        ConfigField::new(
            "cache",
            "max_entries",
            FieldType::Text,
            config.cache.max_entries.to_string(),
            "Maximum number of responses to keep in the LRU cache",
        ),
        // CloudCode section
        ConfigField::new(
            "cloudcode",
            "timeout_secs",
            FieldType::Text,
            config.cloudcode.timeout_secs.to_string(),
            "Timeout in seconds for Google Cloud Code API requests",
        ),
        ConfigField::new(
            "cloudcode",
            "max_retries",
            FieldType::Text,
            config.cloudcode.max_retries.to_string(),
            "Maximum number of retry attempts for failed or rate-limited requests",
        ),
        ConfigField::new(
            "cloudcode",
            "max_concurrent_requests",
            FieldType::Text,
            config.cloudcode.max_concurrent_requests.to_string(),
            "Maximum number of simultaneous requests to the Cloud Code API",
        ),
        ConfigField::new(
            "cloudcode",
            "min_request_interval_ms",
            FieldType::Text,
            config.cloudcode.min_request_interval_ms.to_string(),
            "Minimum delay in milliseconds between consecutive API requests",
        ),
    ]
}
