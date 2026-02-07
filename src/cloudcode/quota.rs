use serde::Deserialize;
use std::collections::HashMap;

use crate::colors::*;
use crate::models::get_model_family;

#[derive(Debug, Deserialize)]
pub struct FetchAvailableModelsResponse {
    pub models: Option<HashMap<String, ModelData>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelData {
    pub quota_info: Option<QuotaInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaInfo {
    pub remaining_fraction: Option<f64>,
    pub reset_time: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelQuota {
    pub model_id: String,
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
}

pub async fn fetch_model_quotas(
    http_client: &crate::auth::HttpClient,
    access_token: &str,
    project_id: Option<&str>,
) -> Result<Vec<ModelQuota>, String> {
    const ENDPOINTS: &[&str] = &[
        "https://daily-cloudcode-pa.googleapis.com",
        "https://cloudcode-pa.googleapis.com",
    ];

    let body = if let Some(pid) = project_id {
        format!(r#"{{"project":"{}"}}"#, pid)
    } else {
        "{}".to_string()
    };

    for endpoint in ENDPOINTS {
        let url = format!("{}/v1internal:fetchAvailableModels", endpoint);

        match http_client
            .post_with_headers(
                &url,
                "application/json",
                body.as_bytes(),
                &[("Authorization", &format!("Bearer {}", access_token))],
            )
            .await
        {
            Ok(response_bytes) => {
                let response: FetchAvailableModelsResponse =
                    serde_json::from_slice(&response_bytes)
                        .map_err(|e| format!("Failed to parse response: {}", e))?;

                let mut quotas = Vec::new();

                if let Some(models) = response.models {
                    for (model_id, model_data) in models {
                        let family = get_model_family(&model_id);
                        if family != "claude" && family != "gemini" {
                            continue;
                        }

                        if let Some(quota_info) = model_data.quota_info {
                            let remaining = quota_info.remaining_fraction.unwrap_or_else(|| {
                                if quota_info.reset_time.is_some() {
                                    0.0
                                } else {
                                    1.0
                                }
                            });

                            quotas.push(ModelQuota {
                                model_id: model_id.clone(),
                                remaining_fraction: remaining,
                                reset_time: quota_info.reset_time,
                            });
                        }
                    }
                }

                quotas.sort_by(|a, b| a.model_id.cmp(&b.model_id));
                return Ok(quotas);
            }
            Err(e) => {
                tracing::debug!(endpoint = %endpoint, error = %e, "fetchAvailableModels failed");
                continue;
            }
        }
    }

    Err("Failed to fetch quotas from all endpoints".to_string())
}

pub fn render_quota_display(quotas: &[ModelQuota]) {
    if quotas.is_empty() {
        println!("{}No quota information available{}", DIM, RESET);
        return;
    }

    // Group by family
    let mut claude_models: Vec<&ModelQuota> = Vec::new();
    let mut gemini_models: Vec<&ModelQuota> = Vec::new();

    for quota in quotas {
        let family = get_model_family(&quota.model_id);
        match family {
            "claude" => claude_models.push(quota),
            "gemini" => gemini_models.push(quota),
            _ => {}
        }
    }

    // Find max model name length for alignment
    let max_name_len = quotas
        .iter()
        .map(|q| q.model_id.len())
        .max()
        .unwrap_or(20)
        .max(25);

    println!();
    println!("{}{}Model Quotas{}", BOLD, CYAN, RESET);
    println!("{}{}", DIM, "─".repeat(max_name_len + 45));
    println!("{}", RESET);

    let render_group = |models: &[&ModelQuota], title: &str| {
        if models.is_empty() {
            return;
        }

        println!("{}{}{}", BOLD, title, RESET);
        println!();

        for quota in models {
            let pct = (quota.remaining_fraction * 100.0).round() as u32;
            let bar_width = 30;
            let filled =
                ((quota.remaining_fraction * bar_width as f64).round() as usize).min(bar_width);
            let empty = bar_width - filled;

            // Color based on percentage
            let color = if pct >= 50 {
                GREEN
            } else if pct >= 20 {
                YELLOW
            } else {
                RED
            };

            // Build the bar
            let bar = format!(
                "{}{}{}{}{}",
                color,
                "█".repeat(filled),
                DIM,
                "░".repeat(empty),
                RESET
            );

            // Format reset time if present
            let reset_info = quota
                .reset_time
                .as_ref()
                .map(|t| format!(" {}(resets: {}){}", DIM, format_reset_time(t), RESET))
                .unwrap_or_default();

            println!(
                "  {:<width$}  {} {:>3}%{}",
                quota.model_id,
                bar,
                pct,
                reset_info,
                width = max_name_len
            );
        }
        println!();
    };

    render_group(&claude_models, "Claude");
    render_group(&gemini_models, "Gemini");
}

fn format_reset_time(reset_time: &str) -> String {
    // Try to parse ISO 8601 timestamp and show relative time
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(reset_time) {
        let now = chrono::Utc::now();
        let duration = parsed.signed_duration_since(now);

        if duration.num_seconds() <= 0 {
            return "now".to_string();
        }

        let hours = duration.num_hours();
        let mins = duration.num_minutes() % 60;
        let secs = duration.num_seconds() % 60;

        if hours > 0 {
            format!("{}h{}m", hours, mins)
        } else if mins > 0 {
            format!("{}m{}s", mins, secs)
        } else {
            format!("{}s", secs)
        }
    } else {
        reset_time.to_string()
    }
}
