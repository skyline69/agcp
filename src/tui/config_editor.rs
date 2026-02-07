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
}

impl ConfigField {
    pub fn new(
        section: &'static str,
        key: &'static str,
        field_type: FieldType,
        value: String,
    ) -> Self {
        Self {
            section,
            key,
            field_type,
            original: value.clone(),
            value,
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
        ),
        ConfigField::new(
            "server",
            "host",
            FieldType::Text,
            config.server.host.clone(),
        ),
        ConfigField::new(
            "server",
            "request_timeout_secs",
            FieldType::Text,
            config.server.request_timeout_secs.to_string(),
        ),
        // Logging section
        ConfigField::new(
            "logging",
            "debug",
            FieldType::Bool,
            config.logging.debug.to_string(),
        ),
        ConfigField::new(
            "logging",
            "log_requests",
            FieldType::Bool,
            config.logging.log_requests.to_string(),
        ),
        // Accounts section
        ConfigField::new(
            "accounts",
            "strategy",
            FieldType::Enum(vec!["sticky", "roundrobin", "hybrid"]),
            config.accounts.strategy.clone(),
        ),
        ConfigField::new(
            "accounts",
            "quota_threshold",
            FieldType::Float { min: 0.0, max: 1.0 },
            config.accounts.quota_threshold.to_string(),
        ),
        ConfigField::new(
            "accounts",
            "fallback",
            FieldType::Bool,
            config.accounts.fallback.to_string(),
        ),
        // Cache section
        ConfigField::new(
            "cache",
            "enabled",
            FieldType::Bool,
            config.cache.enabled.to_string(),
        ),
        ConfigField::new(
            "cache",
            "ttl_seconds",
            FieldType::Text,
            config.cache.ttl_seconds.to_string(),
        ),
        ConfigField::new(
            "cache",
            "max_entries",
            FieldType::Text,
            config.cache.max_entries.to_string(),
        ),
        // CloudCode section
        ConfigField::new(
            "cloudcode",
            "timeout_secs",
            FieldType::Text,
            config.cloudcode.timeout_secs.to_string(),
        ),
        ConfigField::new(
            "cloudcode",
            "max_retries",
            FieldType::Text,
            config.cloudcode.max_retries.to_string(),
        ),
        ConfigField::new(
            "cloudcode",
            "max_concurrent_requests",
            FieldType::Text,
            config.cloudcode.max_concurrent_requests.to_string(),
        ),
        ConfigField::new(
            "cloudcode",
            "min_request_interval_ms",
            FieldType::Text,
            config.cloudcode.min_request_interval_ms.to_string(),
        ),
    ]
}
