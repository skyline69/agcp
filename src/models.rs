use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    // Claude models
    ClaudeOpus4_6Thinking,
    ClaudeOpus4_5Thinking,
    ClaudeSonnet4_5,
    ClaudeSonnet4_5Thinking,
    // Gemini 2.5 models
    Gemini25Flash,
    Gemini25FlashLite,
    Gemini25FlashThinking,
    Gemini25Pro,
    // Gemini 3 models
    Gemini3Flash,
    Gemini3ProHigh,
    Gemini3ProImage,
    Gemini3ProLow,
}

impl Model {
    pub fn anthropic_id(&self) -> &'static str {
        match self {
            Model::ClaudeOpus4_6Thinking => "claude-opus-4-6-thinking",
            Model::ClaudeOpus4_5Thinking => "claude-opus-4-5-thinking",
            Model::ClaudeSonnet4_5 => "claude-sonnet-4-5",
            Model::ClaudeSonnet4_5Thinking => "claude-sonnet-4-5-thinking",
            Model::Gemini25Flash => "gemini-2.5-flash",
            Model::Gemini25FlashLite => "gemini-2.5-flash-lite",
            Model::Gemini25FlashThinking => "gemini-2.5-flash-thinking",
            Model::Gemini25Pro => "gemini-2.5-pro",
            Model::Gemini3Flash => "gemini-3-flash",
            Model::Gemini3ProHigh => "gemini-3-pro-high",
            Model::Gemini3ProImage => "gemini-3-pro-image",
            Model::Gemini3ProLow => "gemini-3-pro-low",
        }
    }

    pub fn all() -> &'static [Model] {
        &[
            Model::ClaudeOpus4_6Thinking,
            Model::ClaudeOpus4_5Thinking,
            Model::ClaudeSonnet4_5,
            Model::ClaudeSonnet4_5Thinking,
            Model::Gemini25Flash,
            Model::Gemini25FlashLite,
            Model::Gemini25FlashThinking,
            Model::Gemini25Pro,
            Model::Gemini3Flash,
            Model::Gemini3ProHigh,
            Model::Gemini3ProImage,
            Model::Gemini3ProLow,
        ]
    }
}

pub fn get_model_family(model_name: &str) -> &'static str {
    let lower = model_name.to_lowercase();
    if lower.contains("claude") {
        "claude"
    } else if lower.contains("gemini") {
        "gemini"
    } else {
        "unknown"
    }
}

/// Resolve model aliases to their full model names.
/// Supports shorthand like "opus", "sonnet", "flash", etc.
pub fn resolve_model_alias(model: &str) -> &str {
    let lower = model.to_lowercase();

    // Handle dated model names (e.g., claude-opus-4-6-20251201 -> claude-opus-4-6-thinking)
    if lower.starts_with("claude-opus-4-6") || lower.starts_with("claude-opus-4.6") {
        return "claude-opus-4-6-thinking";
    }
    if lower.starts_with("claude-opus-4-5") || lower.starts_with("claude-opus-4.5") {
        return "claude-opus-4-5-thinking";
    }
    if lower.starts_with("claude-sonnet-4-5-thinking")
        || lower.starts_with("claude-sonnet-4.5-thinking")
    {
        return "claude-sonnet-4-5-thinking";
    }
    if lower.starts_with("claude-sonnet-4-5") || lower.starts_with("claude-sonnet-4.5") {
        return "claude-sonnet-4-5";
    }

    match lower.as_str() {
        // Claude aliases - default to 4.6, fallback to 4.5 if unavailable
        "opus" | "opus-thinking" | "claude-opus" => "claude-opus-4-6-thinking",
        "opus-4-5" | "opus-4.5" | "claude-opus-4-5" => "claude-opus-4-5-thinking",
        "sonnet" | "claude-sonnet" => "claude-sonnet-4-5",
        "sonnet-thinking" | "claude-sonnet-thinking" => "claude-sonnet-4-5-thinking",
        // Haiku is not available on Cloud Code, map to Gemini 3 Flash
        "haiku" | "claude-haiku" | "claude-haiku-4-5" => "gemini-3-flash",

        // OpenAI-style aliases (for Codex CLI compatibility)
        "gpt-5.2-codex" | "gpt-5.2" | "gpt-5" | "o3" | "o3-high" => "claude-opus-4-6-thinking",

        // Gemini 2.5 aliases
        "flash" | "gemini-flash" => "gemini-2.5-flash",
        "flash-lite" | "gemini-flash-lite" => "gemini-2.5-flash-lite",
        "flash-thinking" | "gemini-flash-thinking" => "gemini-2.5-flash-thinking",
        "pro" | "gemini-pro" => "gemini-2.5-pro",

        // Gemini 3 aliases
        "3-flash" | "gemini3-flash" => "gemini-3-flash",
        "3-pro" | "3-pro-high" | "gemini3-pro" => "gemini-3-pro-high",
        "3-pro-low" | "gemini3-pro-low" => "gemini-3-pro-low",
        "3-pro-image" | "gemini3-pro-image" => "gemini-3-pro-image",

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
        _ => None,
    }
}

/// Claude models need "thinking" in name.
/// Gemini 3+ models are all thinking models (e.g., gemini-3-flash).
pub fn is_thinking_model(model_name: &str) -> bool {
    let lower = model_name.to_lowercase();

    if lower.contains("claude") && lower.contains("thinking") {
        return true;
    }

    if lower.contains("gemini") {
        if lower.contains("thinking") {
            return true;
        }
        // gemini-3+ are all thinking models
        if let Some(version_str) = lower.strip_prefix("gemini-")
            && let Some(version_num) = version_str.chars().next().and_then(|c| c.to_digit(10))
            && version_num >= 3
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_family() {
        assert_eq!(get_model_family("claude-sonnet-4-5"), "claude");
        assert_eq!(get_model_family("gemini-3-flash"), "gemini");
        assert_eq!(get_model_family("unknown-model"), "unknown");
    }

    #[test]
    fn test_is_thinking() {
        // Models with explicit "thinking" in name
        assert!(is_thinking_model("claude-opus-4-6-thinking"));
        assert!(is_thinking_model("claude-opus-4-5-thinking"));
        assert!(is_thinking_model("claude-sonnet-4-5-thinking"));
        assert!(is_thinking_model("gemini-2.5-flash-thinking"));

        // All Gemini 3+ models are thinking models (matches JS behavior)
        assert!(is_thinking_model("gemini-3-flash"));
        assert!(is_thinking_model("gemini-3-pro-high"));
        assert!(is_thinking_model("gemini-4-flash")); // Future-proof

        // Non-thinking models
        assert!(!is_thinking_model("claude-sonnet-4-5")); // No "thinking" in name
        assert!(!is_thinking_model("gemini-2.5-flash")); // Below version 3
        assert!(!is_thinking_model("gemini-2.0-flash")); // Below version 3
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

        // Gemini aliases
        assert_eq!(resolve_model_alias("flash"), "gemini-2.5-flash");
        assert_eq!(resolve_model_alias("pro"), "gemini-2.5-pro");
        assert_eq!(resolve_model_alias("3-flash"), "gemini-3-flash");
        assert_eq!(resolve_model_alias("3-pro"), "gemini-3-pro-high");

        // Full names should pass through unchanged
        assert_eq!(
            resolve_model_alias("claude-opus-4-6-thinking"),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(
            resolve_model_alias("claude-opus-4-5-thinking"),
            "claude-opus-4-5-thinking"
        );
        assert_eq!(resolve_model_alias("gemini-2.5-flash"), "gemini-2.5-flash");

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

        // Unknown models have no fallback
        assert_eq!(get_fallback_model("unknown-model"), None);
    }
}
