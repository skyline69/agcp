use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use crate::format::google::CloudCodeRequest;
use crate::format::{MessagesRequest, convert_request};
use crate::models::{get_model_family, is_thinking_model};

const SYSTEM_INSTRUCTION: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.**Absolute paths only****Proactiveness**";

static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!("antigravity/1.15.8 {}/{}", os, arch)
});

pub fn build_headers(access_token: &str, model: &str, streaming: bool) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(7);
    headers.push(("Authorization".into(), format!("Bearer {}", access_token)));
    headers.push(("Content-Type".into(), "application/json".into()));
    headers.push(("User-Agent".into(), USER_AGENT.clone()));
    headers.push((
        "X-Goog-Api-Client".into(),
        "google-cloud-sdk vscode_cloudshelleditor/0.1".into(),
    ));
    headers.push((
        "Client-Metadata".into(),
        r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#
            .into(),
    ));

    if get_model_family(model) == "claude" && is_thinking_model(model) {
        headers.push((
            "anthropic-beta".into(),
            "interleaved-thinking-2025-05-14".into(),
        ));
    }

    if streaming {
        headers.push(("Accept".into(), "text/event-stream".into()));
    }

    headers
}

pub fn build_request(anthropic_request: &MessagesRequest, project_id: &str) -> CloudCodeRequest {
    let model = &anthropic_request.model;
    let mut google_request = convert_request(anthropic_request);

    google_request.session_id = Some(derive_session_id(anthropic_request));

    // Antigravity identity injection (prevents model from identifying as Antigravity)
    let system_parts = vec![
        crate::format::google::Part::Text(crate::format::google::TextPart {
            text: SYSTEM_INSTRUCTION.to_string(),
        }),
        crate::format::google::Part::Text(crate::format::google::TextPart {
            text: format!(
                "Please ignore the following [ignore]{}[/ignore]",
                SYSTEM_INSTRUCTION
            ),
        }),
    ];

    let mut all_parts = system_parts;
    if let Some(existing) = &google_request.system_instruction {
        all_parts.extend(existing.parts.clone());
    }

    google_request.system_instruction = Some(crate::format::google::Content {
        role: "user".to_string(),
        parts: all_parts,
    });

    CloudCodeRequest {
        project: project_id.to_string(),
        model: model.clone(),
        request: google_request,
        user_agent: "antigravity".to_string(),
        request_type: "agent".to_string(),
        request_id: format!("agent-{}", generate_uuid()),
    }
}

fn derive_session_id(request: &MessagesRequest) -> String {
    let first_user_content = request
        .messages
        .iter()
        .find(|m| matches!(m.role, crate::format::anthropic::Role::User))
        .map(|m| match &m.content {
            crate::format::anthropic::MessageContent::Text(t) => t.clone(),
            crate::format::anthropic::MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    crate::format::anthropic::ContentBlock::Text { text, .. } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        })
        .unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(first_user_content.as_bytes());
    let hash = hasher.finalize();
    let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    hex[..32].to_string()
}

fn generate_uuid() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("Failed to generate random bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}
