use serde::{Deserialize, Serialize};

use crate::config::MappingRule;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    // Claude models
    ClaudeOpus4_6Thinking,
    ClaudeOpus4_5Thinking,
    ClaudeSonnet4_5,
    ClaudeSonnet4_5Thinking,
    // Gemini 3 models
    Gemini3Flash,
    Gemini3ProHigh,
    Gemini3ProLow,
    // GPT-OSS models
    GptOss120bMedium,
}

impl Model {
    pub fn anthropic_id(&self) -> &'static str {
        match self {
            Model::ClaudeOpus4_6Thinking => "claude-opus-4-6-thinking",
            Model::ClaudeOpus4_5Thinking => "claude-opus-4-5-thinking",
            Model::ClaudeSonnet4_5 => "claude-sonnet-4-5",
            Model::ClaudeSonnet4_5Thinking => "claude-sonnet-4-5-thinking",
            Model::Gemini3Flash => "gemini-3-flash",
            Model::Gemini3ProHigh => "gemini-3-pro-high",
            Model::Gemini3ProLow => "gemini-3-pro-low",
            Model::GptOss120bMedium => "gpt-oss-120b-medium",
        }
    }

    pub fn all() -> &'static [Model] {
        &[
            Model::ClaudeOpus4_6Thinking,
            Model::ClaudeOpus4_5Thinking,
            Model::ClaudeSonnet4_5,
            Model::ClaudeSonnet4_5Thinking,
            Model::Gemini3Flash,
            Model::Gemini3ProHigh,
            Model::Gemini3ProLow,
            Model::GptOss120bMedium,
        ]
    }
}

/// Case-insensitive ASCII substring check without allocation.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Case-insensitive ASCII prefix check without allocation.
fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

pub fn get_model_family(model_name: &str) -> &'static str {
    if contains_ignore_case(model_name, "claude") {
        "claude"
    } else if contains_ignore_case(model_name, "gemini") {
        "gemini"
    } else if contains_ignore_case(model_name, "gpt-oss") {
        "gpt-oss"
    } else {
        "unknown"
    }
}

/// Resolve model aliases to their full model names.
/// Supports shorthand like "opus", "sonnet", "flash", etc.
pub fn resolve_model_alias(model: &str) -> &str {
    // Handle dated model names with case-insensitive prefix checks (no allocation)
    if starts_with_ignore_case(model, "claude-opus-4-6")
        || starts_with_ignore_case(model, "claude-opus-4.6")
    {
        return "claude-opus-4-6-thinking";
    }
    if starts_with_ignore_case(model, "claude-opus-4-5")
        || starts_with_ignore_case(model, "claude-opus-4.5")
    {
        return "claude-opus-4-5-thinking";
    }
    if starts_with_ignore_case(model, "claude-sonnet-4-5-thinking")
        || starts_with_ignore_case(model, "claude-sonnet-4.5-thinking")
    {
        return "claude-sonnet-4-5-thinking";
    }
    if starts_with_ignore_case(model, "claude-sonnet-4-5")
        || starts_with_ignore_case(model, "claude-sonnet-4.5")
    {
        return "claude-sonnet-4-5";
    }

    // For alias matching, allocate only if we didn't match above
    let lower = model.to_ascii_lowercase();

    match lower.as_str() {
        // Claude aliases - default to 4.6, fallback to 4.5 if unavailable
        "opus" | "opus-thinking" | "claude-opus" => "claude-opus-4-6-thinking",
        "opus-4-5" | "opus-4.5" | "claude-opus-4-5" => "claude-opus-4-5-thinking",
        "sonnet" | "claude-sonnet" => "claude-sonnet-4-5",
        "sonnet-thinking" | "claude-sonnet-thinking" => "claude-sonnet-4-5-thinking",
        // Haiku is not available, map to Gemini 3 Flash
        "haiku" | "claude-haiku" | "claude-haiku-4-5" => "gemini-3-flash",

        // OpenAI-style aliases (for Codex CLI compatibility)
        "gpt-5.2-codex" | "gpt-5.2" | "gpt-5" | "o3" | "o3-high" => "claude-opus-4-6-thinking",

        // Gemini 2.5 aliases (no longer available, redirect to Gemini 3)
        "flash" | "gemini-flash" => "gemini-3-flash",
        "flash-lite" | "gemini-flash-lite" => "gemini-3-flash",
        "flash-thinking" | "gemini-flash-thinking" => "gemini-3-flash",
        "pro" | "gemini-pro" => "gemini-3-pro-high",

        // Gemini 3 aliases
        "3-flash" | "gemini3-flash" => "gemini-3-flash",
        "3-pro" | "3-pro-high" | "gemini3-pro" => "gemini-3-pro-high",
        "3-pro-low" | "gemini3-pro-low" => "gemini-3-pro-low",

        // GPT-OSS aliases
        "gpt-oss" | "gpt-oss-120b" | "oss" => "gpt-oss-120b-medium",

        // No alias matched, return original
        _ => model,
    }
}

/// Get fallback model for a given model ID.
/// Returns None if no fallback is configured.
pub fn get_fallback_model(model: &str) -> Option<&'static str> {
    match model {
        "gemini-3-pro-high" => Some("claude-opus-4-6-thinking"),
        "gemini-3-pro-low" => Some("claude-sonnet-4-5"),
        "gemini-3-flash" => Some("claude-sonnet-4-5-thinking"),
        // Opus 4.6 falls back to 4.5, which falls back to gemini-3-pro-high
        "claude-opus-4-6-thinking" => Some("claude-opus-4-5-thinking"),
        "claude-opus-4-5-thinking" => Some("gemini-3-pro-high"),
        "claude-sonnet-4-5-thinking" => Some("gemini-3-flash"),
        "claude-sonnet-4-5" => Some("gemini-3-flash"),
        "gpt-oss-120b-medium" => Some("gemini-3-flash"),
        _ => None,
    }
}

/// Claude models need "thinking" in name.
/// Gemini 3+ models are all thinking models (e.g., gemini-3-flash).
pub fn is_thinking_model(model_name: &str) -> bool {
    if contains_ignore_case(model_name, "claude") && contains_ignore_case(model_name, "thinking") {
        return true;
    }

    if contains_ignore_case(model_name, "gemini") {
        if contains_ignore_case(model_name, "thinking") {
            return true;
        }
        // gemini-3+ are all thinking models
        let lower = model_name.to_ascii_lowercase();
        if let Some(version_str) = lower.strip_prefix("gemini-")
            && let Some(version_num) = version_str.chars().next().and_then(|c| c.to_digit(10))
            && version_num >= 3
        {
            return true;
        }
    }

    false
}

/// Simple glob pattern matching supporting `*` as a wildcard.
/// - `*` at end: prefix match (e.g. "gpt-4*" matches "gpt-4o-mini")
/// - `*` at start: suffix match (e.g. "*-thinking" matches "claude-opus-4-5-thinking")
/// - `*` in middle: splits on `*` and checks prefix + suffix
/// - No `*`: exact match (case-insensitive)
pub fn glob_match(pattern: &str, input: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let input = input.to_ascii_lowercase();

    if let Some(idx) = pattern.find('*') {
        let prefix = &pattern[..idx];
        let suffix = &pattern[idx + 1..];

        if suffix.is_empty() {
            // "gpt-4*" — prefix match
            input.starts_with(prefix)
        } else if prefix.is_empty() {
            // "*-thinking" — suffix match
            input.ends_with(suffix)
        } else {
            // "claude-*-opus" — prefix + suffix match
            input.starts_with(prefix)
                && input.ends_with(suffix)
                && input.len() >= prefix.len() + suffix.len()
        }
    } else {
        // Exact match (case-insensitive)
        pattern == input
    }
}

/// Resolve a model name using user-defined mapping rules first,
/// then falling back to the hardcoded alias table.
/// Also handles the background task model substitution.
pub fn resolve_with_mappings(
    model: &str,
    rules: &[MappingRule],
    background_task_model: &str,
) -> String {
    // Check for background task model
    if model == "internal-background-task" {
        return background_task_model.to_string();
    }

    // Check user mappings (first match wins)
    for rule in rules {
        if glob_match(&rule.from, model) {
            return rule.to.clone();
        }
    }

    // Fall through to hardcoded aliases
    resolve_model_alias(model).to_string()
}

/// Available mapping presets
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingPreset {
    None,
    Balanced,
    Performance,
    Cost,
    Custom,
}

impl MappingPreset {
    pub fn name(&self) -> &'static str {
        match self {
            MappingPreset::None => "none",
            MappingPreset::Balanced => "balanced",
            MappingPreset::Performance => "performance",
            MappingPreset::Cost => "cost",
            MappingPreset::Custom => "custom",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            MappingPreset::None => "None",
            MappingPreset::Balanced => "Balanced",
            MappingPreset::Performance => "Performance",
            MappingPreset::Cost => "Cost Optimized",
            MappingPreset::Custom => "Custom",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            MappingPreset::None => "No mappings — pass model names through unchanged",
            MappingPreset::Balanced => "Smart tiering based on model capability class",
            MappingPreset::Performance => "Map everything to the most capable models",
            MappingPreset::Cost => "Map everything to the cheapest capable models",
            MappingPreset::Custom => "User-defined custom mapping rules",
        }
    }

    pub fn from_name(name: &str) -> MappingPreset {
        match name.to_ascii_lowercase().as_str() {
            "balanced" => MappingPreset::Balanced,
            "performance" => MappingPreset::Performance,
            "cost" => MappingPreset::Cost,
            "custom" => MappingPreset::Custom,
            _ => MappingPreset::None,
        }
    }

    pub fn next(&self) -> MappingPreset {
        match self {
            MappingPreset::None => MappingPreset::Balanced,
            MappingPreset::Balanced => MappingPreset::Performance,
            MappingPreset::Performance => MappingPreset::Cost,
            MappingPreset::Cost => MappingPreset::Custom,
            MappingPreset::Custom => MappingPreset::None,
        }
    }

    /// Get the default rules for this preset
    pub fn rules(&self) -> Vec<MappingRule> {
        match self {
            MappingPreset::Balanced => vec![
                MappingRule {
                    from: "claude-3-haiku-*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "claude-haiku-*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "gpt-4o*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "gpt-4*".into(),
                    to: "gemini-3-pro-high".into(),
                },
                MappingRule {
                    from: "gpt-3.5*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "o1-*".into(),
                    to: "gemini-3-pro-high".into(),
                },
                MappingRule {
                    from: "o3-*".into(),
                    to: "gemini-3-pro-high".into(),
                },
                MappingRule {
                    from: "claude-3-opus-*".into(),
                    to: "claude-opus-4-6-thinking".into(),
                },
                MappingRule {
                    from: "claude-3-5-sonnet-*".into(),
                    to: "claude-sonnet-4-5".into(),
                },
                MappingRule {
                    from: "claude-opus-4-*".into(),
                    to: "claude-opus-4-6-thinking".into(),
                },
            ],
            MappingPreset::Performance => vec![
                MappingRule {
                    from: "claude-3-haiku-*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "claude-haiku-*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "gpt-4o*".into(),
                    to: "gemini-3-pro-high".into(),
                },
                MappingRule {
                    from: "gpt-4*".into(),
                    to: "gemini-3-pro-high".into(),
                },
                MappingRule {
                    from: "gpt-3.5*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "o1-*".into(),
                    to: "claude-opus-4-6-thinking".into(),
                },
                MappingRule {
                    from: "o3-*".into(),
                    to: "claude-opus-4-6-thinking".into(),
                },
                MappingRule {
                    from: "claude-3-opus-*".into(),
                    to: "claude-opus-4-6-thinking".into(),
                },
                MappingRule {
                    from: "claude-3-5-sonnet-*".into(),
                    to: "claude-sonnet-4-5-thinking".into(),
                },
                MappingRule {
                    from: "claude-opus-4-*".into(),
                    to: "claude-opus-4-6-thinking".into(),
                },
            ],
            MappingPreset::Cost => vec![
                MappingRule {
                    from: "claude-3-haiku-*".into(),
                    to: "gpt-oss-120b-medium".into(),
                },
                MappingRule {
                    from: "claude-haiku-*".into(),
                    to: "gpt-oss-120b-medium".into(),
                },
                MappingRule {
                    from: "gpt-4o*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "gpt-4*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "gpt-3.5*".into(),
                    to: "gpt-oss-120b-medium".into(),
                },
                MappingRule {
                    from: "o1-*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "o3-*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "claude-3-opus-*".into(),
                    to: "claude-sonnet-4-5".into(),
                },
                MappingRule {
                    from: "claude-3-5-sonnet-*".into(),
                    to: "gemini-3-flash".into(),
                },
                MappingRule {
                    from: "claude-opus-4-*".into(),
                    to: "claude-sonnet-4-5".into(),
                },
            ],
            MappingPreset::None | MappingPreset::Custom => vec![],
        }
    }
}

/// Get a list of all available target model IDs for use in the mappings UI
pub fn all_target_models() -> Vec<&'static str> {
    Model::all().iter().map(|m| m.anthropic_id()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_family() {
        assert_eq!(get_model_family("claude-sonnet-4-5"), "claude");
        assert_eq!(get_model_family("gemini-3-flash"), "gemini");
        assert_eq!(get_model_family("gpt-oss-120b-medium"), "gpt-oss");
        assert_eq!(get_model_family("unknown-model"), "unknown");
    }

    #[test]
    fn test_is_thinking() {
        // Models with explicit "thinking" in name
        assert!(is_thinking_model("claude-opus-4-6-thinking"));
        assert!(is_thinking_model("claude-opus-4-5-thinking"));
        assert!(is_thinking_model("claude-sonnet-4-5-thinking"));

        // All Gemini 3+ models are thinking models (matches JS behavior)
        assert!(is_thinking_model("gemini-3-flash"));
        assert!(is_thinking_model("gemini-3-pro-high"));
        assert!(is_thinking_model("gemini-4-flash")); // Future-proof

        // Non-thinking models
        assert!(!is_thinking_model("claude-sonnet-4-5")); // No "thinking" in name
        assert!(!is_thinking_model("gpt-oss-120b-medium")); // Not a thinking model
    }

    #[test]
    fn test_model_aliases() {
        // Claude aliases - opus now defaults to 4.6
        assert_eq!(resolve_model_alias("opus"), "claude-opus-4-6-thinking");
        assert_eq!(resolve_model_alias("opus-4-5"), "claude-opus-4-5-thinking");
        assert_eq!(resolve_model_alias("sonnet"), "claude-sonnet-4-5");
        assert_eq!(
            resolve_model_alias("sonnet-thinking"),
            "claude-sonnet-4-5-thinking"
        );

        // Gemini aliases (2.5 aliases now redirect to Gemini 3)
        assert_eq!(resolve_model_alias("flash"), "gemini-3-flash");
        assert_eq!(resolve_model_alias("pro"), "gemini-3-pro-high");
        assert_eq!(resolve_model_alias("3-flash"), "gemini-3-flash");
        assert_eq!(resolve_model_alias("3-pro"), "gemini-3-pro-high");

        // GPT-OSS aliases
        assert_eq!(resolve_model_alias("gpt-oss"), "gpt-oss-120b-medium");
        assert_eq!(resolve_model_alias("oss"), "gpt-oss-120b-medium");

        // Full names should pass through unchanged
        assert_eq!(
            resolve_model_alias("claude-opus-4-6-thinking"),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(
            resolve_model_alias("claude-opus-4-5-thinking"),
            "claude-opus-4-5-thinking"
        );
        assert_eq!(resolve_model_alias("gemini-3-flash"), "gemini-3-flash");
        assert_eq!(
            resolve_model_alias("gpt-oss-120b-medium"),
            "gpt-oss-120b-medium"
        );

        // Unknown models pass through
        assert_eq!(resolve_model_alias("unknown-model"), "unknown-model");
    }

    #[test]
    fn test_fallback_models() {
        // Gemini to Claude fallbacks - default to 4.6
        assert_eq!(
            get_fallback_model("gemini-3-pro-high"),
            Some("claude-opus-4-6-thinking")
        );
        assert_eq!(
            get_fallback_model("gemini-3-flash"),
            Some("claude-sonnet-4-5-thinking")
        );

        // Claude model fallbacks: 4.6 -> 4.5 -> gemini
        assert_eq!(
            get_fallback_model("claude-opus-4-6-thinking"),
            Some("claude-opus-4-5-thinking")
        );
        assert_eq!(
            get_fallback_model("claude-opus-4-5-thinking"),
            Some("gemini-3-pro-high")
        );
        assert_eq!(
            get_fallback_model("claude-sonnet-4-5-thinking"),
            Some("gemini-3-flash")
        );

        // GPT-OSS fallback
        assert_eq!(
            get_fallback_model("gpt-oss-120b-medium"),
            Some("gemini-3-flash")
        );

        // Unknown models have no fallback
        assert_eq!(get_fallback_model("unknown-model"), None);
    }

    #[test]
    fn test_glob_match() {
        // Suffix wildcard (prefix match)
        assert!(glob_match("gpt-4*", "gpt-4"));
        assert!(glob_match("gpt-4*", "gpt-4o"));
        assert!(glob_match("gpt-4*", "gpt-4o-mini"));
        assert!(glob_match("gpt-4*", "GPT-4O")); // case-insensitive
        assert!(!glob_match("gpt-4*", "gpt-3.5-turbo"));

        // Prefix wildcard (suffix match)
        assert!(glob_match("*-thinking", "claude-opus-4-5-thinking"));
        assert!(!glob_match("*-thinking", "claude-sonnet-4-5"));

        // Middle wildcard
        assert!(glob_match("claude-*-thinking", "claude-opus-4-5-thinking"));
        assert!(!glob_match("claude-*-thinking", "claude-sonnet-4-5"));

        // Exact match
        assert!(glob_match("gpt-4", "gpt-4"));
        assert!(glob_match("GPT-4", "gpt-4")); // case-insensitive
        assert!(!glob_match("gpt-4", "gpt-4o"));

        // Real-world patterns
        assert!(glob_match("claude-3-haiku-*", "claude-3-haiku-20240307"));
        assert!(glob_match("o1-*", "o1-preview"));
        assert!(glob_match("o3-*", "o3-mini"));
        assert!(glob_match("claude-opus-4-*", "claude-opus-4-5-thinking"));
    }

    #[test]
    fn test_resolve_with_mappings() {
        let rules = vec![
            MappingRule {
                from: "gpt-4*".into(),
                to: "gemini-3-pro-high".into(),
            },
            MappingRule {
                from: "claude-3-haiku-*".into(),
                to: "gemini-3-flash".into(),
            },
        ];

        // User mapping takes priority
        assert_eq!(
            resolve_with_mappings("gpt-4o", &rules, "gemini-3-flash"),
            "gemini-3-pro-high"
        );
        assert_eq!(
            resolve_with_mappings("claude-3-haiku-20240307", &rules, "gemini-3-flash"),
            "gemini-3-flash"
        );

        // No user mapping match -> falls through to hardcoded aliases
        assert_eq!(
            resolve_with_mappings("opus", &rules, "gemini-3-flash"),
            "claude-opus-4-6-thinking"
        );

        // Background task model
        assert_eq!(
            resolve_with_mappings("internal-background-task", &rules, "gemini-3-flash"),
            "gemini-3-flash"
        );

        // Unknown model passes through
        assert_eq!(
            resolve_with_mappings("totally-unknown", &rules, "gemini-3-flash"),
            "totally-unknown"
        );
    }

    #[test]
    fn test_mapping_presets() {
        // Balanced preset has rules
        let balanced = MappingPreset::Balanced.rules();
        assert!(!balanced.is_empty());
        assert!(balanced.iter().any(|r| r.from == "gpt-4*"));

        // Performance preset has rules
        let perf = MappingPreset::Performance.rules();
        assert!(!perf.is_empty());

        // Cost preset has rules
        let cost = MappingPreset::Cost.rules();
        assert!(!cost.is_empty());

        // None and Custom have no rules
        assert!(MappingPreset::None.rules().is_empty());
        assert!(MappingPreset::Custom.rules().is_empty());

        // from_name round-trip
        assert_eq!(
            MappingPreset::from_name("balanced"),
            MappingPreset::Balanced
        );
        assert_eq!(
            MappingPreset::from_name("performance"),
            MappingPreset::Performance
        );
        assert_eq!(MappingPreset::from_name("cost"), MappingPreset::Cost);
        assert_eq!(MappingPreset::from_name("custom"), MappingPreset::Custom);
        assert_eq!(MappingPreset::from_name("none"), MappingPreset::None);
        assert_eq!(MappingPreset::from_name("unknown"), MappingPreset::None);

        // Preset cycling
        assert_eq!(MappingPreset::None.next(), MappingPreset::Balanced);
        assert_eq!(MappingPreset::Balanced.next(), MappingPreset::Performance);
        assert_eq!(MappingPreset::Custom.next(), MappingPreset::None);
    }
}
