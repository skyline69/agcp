use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::auth::HttpClient;
use crate::error::{AuthError, Error, Result};

const LOAD_CODE_ASSIST_ENDPOINTS: &[&str] = &[
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.googleapis.com",
];

#[derive(Debug, Serialize)]
struct LoadCodeAssistRequest {
    metadata: LoadCodeAssistMetadata,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadCodeAssistMetadata {
    ide_type: &'static str,
    platform: &'static str,
    plugin_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    duet_project: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadCodeAssistResponse {
    #[serde(default)]
    cloudaicompanion_project: Option<CloudAIProject>,
    #[serde(default)]
    paid_tier: Option<TierInfo>,
    #[serde(default)]
    current_tier: Option<TierInfo>,
    #[serde(default)]
    allowed_tiers: Option<Vec<TierInfo>>,
}

#[derive(Debug, Deserialize)]
struct TierInfo {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "isDefault")]
    is_default: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CloudAIProject {
    String(String),
    Object { id: String },
}

impl CloudAIProject {
    fn get_id(&self) -> &str {
        match self {
            CloudAIProject::String(s) => s,
            CloudAIProject::Object { id } => id,
        }
    }
}

/// Result from loadCodeAssist containing project and subscription info
#[derive(Debug, Clone)]
pub struct LoadCodeAssistResult {
    pub project_id: Option<String>,
    pub subscription_tier: Option<String>,
}

/// Parse tier ID string to a normalized tier name
fn parse_tier_id(tier_id: &str) -> Option<&'static str> {
    let lower = tier_id.to_lowercase();

    if lower.contains("ultra") {
        Some("ultra")
    } else if lower == "standard-tier" {
        // standard-tier = "Gemini Code Assist" (paid, project-based)
        Some("pro")
    } else if lower.contains("pro") || lower.contains("premium") {
        Some("pro")
    } else if lower == "free-tier" || lower.contains("free") {
        Some("free")
    } else {
        None
    }
}

/// Extract subscription tier from LoadCodeAssist response
fn extract_subscription_tier(data: &LoadCodeAssistResponse) -> Option<String> {
    // Priority: paidTier > currentTier > allowedTiers

    // 1. Check paidTier first (Google One AI subscription - most reliable)
    if let Some(ref tier) = data.paid_tier
        && let Some(ref id) = tier.id
        && let Some(tier_name) = parse_tier_id(id)
    {
        debug!(tier_id = %id, tier = %tier_name, source = "paidTier", "Detected subscription tier");
        return Some(tier_name.to_string());
    }

    // 2. Fall back to currentTier
    if let Some(ref tier) = data.current_tier
        && let Some(ref id) = tier.id
        && let Some(tier_name) = parse_tier_id(id)
    {
        debug!(tier_id = %id, tier = %tier_name, source = "currentTier", "Detected subscription tier");
        return Some(tier_name.to_string());
    }

    // 3. Fall back to allowedTiers (find the default or first non-free tier)
    if let Some(ref tiers) = data.allowed_tiers {
        // First look for the default tier
        let default_tier = tiers.iter().find(|t| t.is_default == Some(true));
        let tier = default_tier.or_else(|| tiers.first());

        if let Some(tier) = tier
            && let Some(ref id) = tier.id
            && let Some(tier_name) = parse_tier_id(id)
        {
            debug!(tier_id = %id, tier = %tier_name, source = "allowedTiers", "Detected subscription tier");
            return Some(tier_name.to_string());
        }
    }

    None
}

/// Discover project ID and subscription tier from loadCodeAssist API
pub async fn discover_project_and_tier(
    http_client: &HttpClient,
    access_token: &str,
    existing_project_id: Option<&str>,
) -> Result<LoadCodeAssistResult> {
    let request_body = LoadCodeAssistRequest {
        metadata: LoadCodeAssistMetadata {
            ide_type: "IDE_UNSPECIFIED",
            platform: "PLATFORM_UNSPECIFIED",
            plugin_type: "GEMINI",
            duet_project: existing_project_id.map(String::from),
        },
    };

    let body_bytes = serde_json::to_vec(&request_body)?;
    let mut last_error: Option<String> = None;

    for endpoint in LOAD_CODE_ASSIST_ENDPOINTS {
        let url = format!("{}/v1internal:loadCodeAssist", endpoint);
        debug!(endpoint = %endpoint, "Calling loadCodeAssist");

        match http_client
            .post_with_auth(&url, access_token, "application/json", &body_bytes)
            .await
        {
            Ok(response_bytes) => {
                let body = String::from_utf8_lossy(&response_bytes).to_string();
                debug!(endpoint = %endpoint, response = %body.chars().take(500).collect::<String>(), "loadCodeAssist response");

                // Parse response
                if let Ok(data) = serde_json::from_str::<LoadCodeAssistResponse>(&body) {
                    let project_id = data
                        .cloudaicompanion_project
                        .as_ref()
                        .map(|p| p.get_id().to_string());
                    let subscription_tier = extract_subscription_tier(&data);

                    if project_id.is_some() {
                        info!(project_id = ?project_id, subscription_tier = ?subscription_tier, "Discovered from loadCodeAssist");
                        return Ok(LoadCodeAssistResult {
                            project_id,
                            subscription_tier,
                        });
                    }

                    // No project in response - account may need onboarding
                    // But we might still have subscription tier info
                    if subscription_tier.is_some() {
                        warn!("No project in loadCodeAssist response, but found subscription tier");
                        return Ok(LoadCodeAssistResult {
                            project_id: existing_project_id.map(String::from),
                            subscription_tier,
                        });
                    }

                    warn!("No project in loadCodeAssist response, account may need onboarding");
                }

                last_error = Some("No project in response".to_string());
            }
            Err(e) => {
                warn!(endpoint = %endpoint, error = %e, "loadCodeAssist request error");
                last_error = Some(e);
            }
        }
    }

    // If we have an existing project ID, use it as fallback
    if let Some(project_id) = existing_project_id {
        warn!(
            "loadCodeAssist failed, using existing project ID: {}",
            project_id
        );
        return Ok(LoadCodeAssistResult {
            project_id: Some(project_id.to_string()),
            subscription_tier: None,
        });
    }

    Err(Error::Auth(AuthError::RefreshFailed(format!(
        "Failed to discover project: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))))
}
