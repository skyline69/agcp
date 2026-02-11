use crate::format::anthropic::{
    ContentBlock, Message, MessageContent, MessagesRequest, Role, SystemPrompt, Tool, ToolChoice,
};
use crate::format::google::{
    Content, FunctionCall, FunctionCallPart, FunctionCallingConfig, FunctionDeclaration,
    FunctionResponse, FunctionResponsePart, GenerateContentRequest, GenerationConfig, GoogleTool,
    InlineData, InlineDataPart, Part, TextPart, ThinkingConfig, ThoughtPart, ToolConfig,
};
use crate::format::signature_cache::{
    get_cached_tool_signature, is_signature_compatible, ModelFamily, GEMINI_SKIP_SIGNATURE,
    MIN_SIGNATURE_LENGTH,
};
use crate::models::{get_model_family, is_thinking_model};

/// Cloud Code API max output token limits per model family.
/// Requests exceeding these are silently capped to avoid 400 errors.
const CLAUDE_MAX_OUTPUT_TOKENS: u32 = 64000;
const GEMINI_MAX_OUTPUT_TOKENS: u32 = 65536;

pub fn convert_request(request: &MessagesRequest) -> GenerateContentRequest {
    let is_thinking = is_thinking_model(&request.model);
    let model_family = get_model_family(&request.model);
    let target_family = ModelFamily::from_str(model_family);

    let contents = convert_messages(&request.messages, target_family);
    let system_instruction = request.system.as_ref().map(convert_system_prompt);

    let thinking_config = if is_thinking {
        // If client explicitly provided thinking config, respect it
        let budget = request.thinking.as_ref().and_then(|t| match t {
            crate::format::anthropic::ThinkingConfig::Enabled { budget_tokens } => *budget_tokens,
            crate::format::anthropic::ThinkingConfig::Disabled => None,
        });

        // Check if client explicitly disabled thinking
        let disabled = matches!(
            request.thinking,
            Some(crate::format::anthropic::ThinkingConfig::Disabled)
        );

        if disabled {
            None
        } else {
            match model_family {
                "claude" => Some(ThinkingConfig::Claude {
                    include_thoughts: true,
                }),
                "gemini" => Some(ThinkingConfig::Gemini {
                    include_thoughts: true,
                    thinking_budget: budget.unwrap_or(16000),
                }),
                _ => None,
            }
        }
    } else {
        // Non-thinking model, but client may have requested thinking explicitly
        match &request.thinking {
            Some(crate::format::anthropic::ThinkingConfig::Enabled { budget_tokens }) => {
                match model_family {
                    "claude" => Some(ThinkingConfig::Claude {
                        include_thoughts: true,
                    }),
                    "gemini" => Some(ThinkingConfig::Gemini {
                        include_thoughts: true,
                        thinking_budget: budget_tokens.unwrap_or(16000),
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    };

    // Claude thinking models reject explicit temperature/top_p/top_k
    // (they use fixed temperature=1.0 internally)
    let (temperature, top_p, top_k) = if is_thinking && model_family == "claude" {
        (None, None, None)
    } else {
        (request.temperature, request.top_p, request.top_k)
    };

    // Cap max_output_tokens to Cloud Code API limits
    let max_tokens = match model_family {
        "claude" => request.max_tokens.min(CLAUDE_MAX_OUTPUT_TOKENS),
        "gemini" => request.max_tokens.min(GEMINI_MAX_OUTPUT_TOKENS),
        _ => request.max_tokens,
    };

    let generation_config = Some(GenerationConfig {
        max_output_tokens: Some(max_tokens),
        temperature,
        top_p,
        top_k,
        stop_sequences: request.stop_sequences.clone(),
        thinking_config,
        response_mime_type: match &request.response_format {
            Some(crate::format::anthropic::ResponseFormatInternal::JsonObject) => {
                Some("application/json".to_string())
            }
            Some(crate::format::anthropic::ResponseFormatInternal::JsonSchema { .. }) => {
                Some("application/json".to_string())
            }
            None => None,
        },
        response_schema: match &request.response_format {
            Some(crate::format::anthropic::ResponseFormatInternal::JsonSchema { schema }) => {
                Some(sanitize_schema(schema))
            }
            _ => None,
        },
        candidate_count: request.candidate_count,
    });

    let tools = request.tools.as_ref().and_then(|t| {
        if t.is_empty() {
            None
        } else {
            Some(convert_tools(t))
        }
    });

    let tool_config = request.tool_choice.as_ref().map(convert_tool_choice);

    GenerateContentRequest {
        contents,
        system_instruction,
        generation_config,
        tools,
        tool_config,
        session_id: None,
    }
}

fn convert_messages(messages: &[Message], target_family: Option<ModelFamily>) -> Vec<Content> {
    messages
        .iter()
        .map(|m| convert_message(m, target_family))
        .collect()
}

fn convert_message(message: &Message, target_family: Option<ModelFamily>) -> Content {
    let role = match message.role {
        Role::User => "user".to_string(),
        Role::Assistant => "model".to_string(),
    };

    let parts = match &message.content {
        MessageContent::Text(text) => {
            vec![Part::Text(TextPart { text: text.clone() })]
        }
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| convert_content_block(b, target_family))
            .collect(),
    };

    Content { role, parts }
}

fn convert_content_block(block: &ContentBlock, target_family: Option<ModelFamily>) -> Option<Part> {
    match block {
        ContentBlock::Text { text, .. } => Some(Part::Text(TextPart { text: text.clone() })),
        ContentBlock::Image { source } => Some(Part::InlineData(InlineDataPart {
            inline_data: InlineData {
                mime_type: source.media_type.clone(),
                data: source.data.clone(),
            },
        })),
        ContentBlock::Document { source, .. } => Some(Part::InlineData(InlineDataPart {
            inline_data: InlineData {
                mime_type: source.media_type.clone(),
                data: source.data.clone(),
            },
        })),
        ContentBlock::ToolUse { id, name, input } => {
            // For Gemini models, we need to include thoughtSignature
            let thought_signature = if target_family == Some(ModelFamily::Gemini) {
                // Try to restore from cache, fall back to skip signature
                get_cached_tool_signature(id).unwrap_or_else(|| GEMINI_SKIP_SIGNATURE.to_string())
            } else {
                // Claude doesn't need thoughtSignature
                String::new()
            };

            Some(Part::FunctionCall(FunctionCallPart {
                function_call: FunctionCall {
                    name: name.clone(),
                    args: input.clone(),
                    id: Some(id.clone()),
                },
                thought_signature: if thought_signature.is_empty() {
                    None
                } else {
                    Some(thought_signature)
                },
            }))
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let response_value = match content {
                crate::format::anthropic::ToolResultContent::Text(text) => {
                    if is_error.unwrap_or(false) {
                        serde_json::json!({ "error": text })
                    } else {
                        serde_json::json!({ "result": text })
                    }
                }
                crate::format::anthropic::ToolResultContent::Blocks(blocks) => {
                    // Concatenate text blocks
                    let text: String = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if is_error.unwrap_or(false) {
                        serde_json::json!({ "error": text })
                    } else {
                        serde_json::json!({ "result": text })
                    }
                }
            };

            Some(Part::FunctionResponse(FunctionResponsePart {
                function_response: FunctionResponse {
                    name: tool_use_id.clone(), // Use tool_use_id as name (fallback)
                    response: response_value,
                    id: Some(tool_use_id.clone()), // ID must match tool_use_id for Claude
                },
            }))
        }
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            // Check signature compatibility for cross-model scenarios
            if let (Some(sig), Some(target)) = (signature.as_ref(), target_family) {
                // For Gemini targets, check if the signature is compatible
                if !is_signature_compatible(sig, target) {
                    // Incompatible signature - drop this thinking block
                    return None;
                }
            }

            // Check minimum signature length
            let valid_signature = signature
                .as_ref()
                .filter(|s| s.len() >= MIN_SIGNATURE_LENGTH)
                .cloned();

            // Convert to thought part
            Some(Part::Thought(ThoughtPart {
                thought: true,
                text: thinking.clone(),
                thought_signature: valid_signature,
            }))
        }
    }
}

fn convert_system_prompt(system: &SystemPrompt) -> Content {
    let parts = match system {
        SystemPrompt::Text(text) => {
            vec![Part::Text(TextPart { text: text.clone() })]
        }
        // System prompts don't need signature handling - pass None for target family
        SystemPrompt::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| convert_content_block(b, None))
            .collect(),
    };

    // Google API uses "user" role for system instructions
    Content {
        role: "user".to_string(),
        parts,
    }
}

fn convert_tools(tools: &[Tool]) -> Vec<GoogleTool> {
    let declarations: Vec<FunctionDeclaration> = tools
        .iter()
        .map(|tool| FunctionDeclaration {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: Some(sanitize_schema(&tool.input_schema)),
        })
        .collect();

    vec![GoogleTool {
        function_declarations: declarations,
    }]
}

/// Convert Anthropic tool_choice to Google's ToolConfig.
fn convert_tool_choice(choice: &ToolChoice) -> ToolConfig {
    match choice {
        ToolChoice::Auto => ToolConfig {
            function_calling_config: FunctionCallingConfig {
                mode: "AUTO".to_string(),
                allowed_function_names: None,
            },
        },
        ToolChoice::Any => ToolConfig {
            function_calling_config: FunctionCallingConfig {
                mode: "ANY".to_string(),
                allowed_function_names: None,
            },
        },
        ToolChoice::Tool { name } => ToolConfig {
            function_calling_config: FunctionCallingConfig {
                mode: "ANY".to_string(),
                allowed_function_names: Some(vec![name.clone()]),
            },
        },
    }
}

// Allowlist sanitizer - Cloud Code API only accepts a subset of JSON Schema
fn sanitize_schema(schema: &serde_json::Value) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(obj) => {
            // Handle anyOf/oneOf by flattening to the first non-null variant
            if let Some(serde_json::Value::Array(arr)) =
                obj.get("anyOf").or_else(|| obj.get("oneOf"))
            {
                // Find first non-null variant schema
                let non_null = arr
                    .iter()
                    .find(|v| v.get("type").and_then(|t| t.as_str()) != Some("null"));
                if let Some(variant) = non_null {
                    // Merge description from parent if variant doesn't have one
                    let mut merged = variant.clone();
                    if let (Some(desc), Some(obj_mut)) =
                        (obj.get("description"), merged.as_object_mut())
                    {
                        obj_mut.entry("description").or_insert_with(|| desc.clone());
                    }
                    return sanitize_schema(&merged);
                }
                // All null or empty - fall through to default handling
            }

            const ALLOWED_FIELDS: &[&str] = &[
                "type",
                "description",
                "properties",
                "required",
                "items",
                "enum",
            ];

            let mut clean = serde_json::Map::new();

            for (key, value) in obj {
                // Convert "const" to "enum" for compatibility
                if key == "const" {
                    clean.insert("enum".to_string(), serde_json::json!([value]));
                    continue;
                }

                // Skip fields not in allowlist
                if !ALLOWED_FIELDS.contains(&key.as_str()) {
                    continue;
                }

                // Recursively sanitize nested structures
                match key.as_str() {
                    // Handle "type": ["string", "null"] -> "type": "string"
                    "type" => {
                        let sanitized_type = match value {
                            serde_json::Value::Array(arr) => {
                                // Pick the first non-null type
                                arr.iter()
                                    .find(|t| t.as_str() != Some("null"))
                                    .cloned()
                                    .unwrap_or_else(|| serde_json::json!("string"))
                            }
                            other => other.clone(),
                        };
                        clean.insert(key.clone(), sanitized_type);
                    }
                    "properties" => {
                        if let serde_json::Value::Object(props) = value {
                            let mut sanitized_props = serde_json::Map::new();
                            for (prop_key, prop_value) in props {
                                sanitized_props
                                    .insert(prop_key.clone(), sanitize_schema(prop_value));
                            }
                            clean.insert(key.clone(), serde_json::Value::Object(sanitized_props));
                        }
                    }
                    "items" => match value {
                        serde_json::Value::Array(arr) => {
                            let sanitized: Vec<_> = arr.iter().map(sanitize_schema).collect();
                            clean.insert(key.clone(), serde_json::Value::Array(sanitized));
                        }
                        serde_json::Value::Object(_) => {
                            clean.insert(key.clone(), sanitize_schema(value));
                        }
                        _ => {
                            clean.insert(key.clone(), value.clone());
                        }
                    },
                    _ => {
                        clean.insert(key.clone(), value.clone());
                    }
                }
            }

            // Ensure we have at least a type
            if !clean.contains_key("type") {
                clean.insert("type".to_string(), serde_json::json!("object"));
            }

            // If object type with no properties, add placeholder
            if clean.get("type") == Some(&serde_json::json!("object"))
                && (!clean.contains_key("properties")
                    || clean
                        .get("properties")
                        .and_then(|p| p.as_object())
                        .map(|o| o.is_empty())
                        .unwrap_or(true))
            {
                clean.insert(
                    "properties".to_string(),
                    serde_json::json!({
                        "reason": {
                            "type": "string",
                            "description": "Reason for calling this tool"
                        }
                    }),
                );
                clean.insert("required".to_string(), serde_json::json!(["reason"]));
            }

            // Validate that required array only contains properties that exist
            if let (
                Some(serde_json::Value::Array(required)),
                Some(serde_json::Value::Object(props)),
            ) = (clean.get("required"), clean.get("properties"))
            {
                let valid_required: Vec<_> = required
                    .iter()
                    .filter(|r| r.as_str().map(|s| props.contains_key(s)).unwrap_or(false))
                    .cloned()
                    .collect();
                if valid_required.is_empty() {
                    clean.remove("required");
                } else {
                    clean.insert(
                        "required".to_string(),
                        serde_json::Value::Array(valid_required),
                    );
                }
            }

            serde_json::Value::Object(clean)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sanitize_schema).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_request(model: &str, content: &str) -> MessagesRequest {
        MessagesRequest {
            model: model.to_string(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(content.to_string()),
            }],
            max_tokens: 1024,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            response_format: None,
            candidate_count: None,
        }
    }

    #[test]
    fn test_convert_simple_request() {
        let request = create_test_request("claude-sonnet-4-5", "Hello");
        let google_req = convert_request(&request);

        assert_eq!(google_req.contents.len(), 1);
        assert_eq!(google_req.contents[0].role, "user");
        assert!(google_req.generation_config.is_some());

        let gen_config = google_req.generation_config.unwrap();
        assert_eq!(gen_config.max_output_tokens, Some(1024));
        assert!(gen_config.thinking_config.is_none()); // Non-thinking model
    }

    #[test]
    fn test_convert_thinking_model_request() {
        let request = create_test_request("claude-opus-4-5-thinking", "Think about this");
        let google_req = convert_request(&request);

        let gen_config = google_req.generation_config.unwrap();
        assert!(gen_config.thinking_config.is_some());

        match gen_config.thinking_config.unwrap() {
            ThinkingConfig::Claude { include_thoughts } => {
                assert!(include_thoughts);
            }
            _ => panic!("Expected Claude thinking config"),
        }
    }

    #[test]
    fn test_convert_gemini_thinking_model() {
        let request = create_test_request("gemini-3-flash", "Process this");
        let google_req = convert_request(&request);

        let gen_config = google_req.generation_config.unwrap();
        assert!(gen_config.thinking_config.is_some());

        match gen_config.thinking_config.unwrap() {
            ThinkingConfig::Gemini {
                include_thoughts,
                thinking_budget,
            } => {
                assert!(include_thoughts);
                assert_eq!(thinking_budget, 16000);
            }
            _ => panic!("Expected Gemini thinking config"),
        }
    }

    #[test]
    fn test_convert_system_prompt() {
        let mut request = create_test_request("claude-sonnet-4-5", "Hello");
        request.system = Some(SystemPrompt::Text(
            "You are a helpful assistant".to_string(),
        ));

        let google_req = convert_request(&request);
        assert!(google_req.system_instruction.is_some());

        let sys = google_req.system_instruction.unwrap();
        assert_eq!(sys.parts.len(), 1);
    }

    #[test]
    fn test_convert_with_tools() {
        let mut request = create_test_request("claude-sonnet-4-5", "Use the tool");
        request.tools = Some(vec![Tool {
            name: "get_weather".to_string(),
            description: Some("Get weather for a location".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                },
                "required": ["location"]
            }),
        }]);

        let google_req = convert_request(&request);
        assert!(google_req.tools.is_some());

        let tools = google_req.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function_declarations.len(), 1);
        assert_eq!(tools[0].function_declarations[0].name, "get_weather");
    }

    #[test]
    fn test_tool_use_in_history_gets_skip_signature_for_gemini() {
        // Create a request with tool use in the conversation history
        let mut request = create_test_request("gemini-3-flash", "Continue");
        request.messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("Use a tool".to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "toolu_test123".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"location": "NYC"}),
                }]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_test123".to_string(),
                    content: crate::format::anthropic::ToolResultContent::Text(
                        "Sunny, 72F".to_string(),
                    ),
                    is_error: None,
                }]),
            },
        ];

        let google_req = convert_request(&request);

        // Find the function call part and verify it has a thought_signature
        let assistant_msg = &google_req.contents[1];
        assert_eq!(assistant_msg.role, "model");

        let has_signature = assistant_msg.parts.iter().any(|p| {
            if let Part::FunctionCall(fc) = p {
                // Should have the skip signature since there's no cached signature
                fc.thought_signature.as_deref() == Some(GEMINI_SKIP_SIGNATURE)
            } else {
                false
            }
        });

        assert!(
            has_signature,
            "FunctionCall should have skip_thought_signature_validator for Gemini models"
        );
    }

    #[test]
    fn test_sanitize_schema_array_type() {
        // Zed sends "type": ["string", "null"] for nullable params
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "description": {"type": ["string", "null"]}
            },
            "required": ["name"]
        });

        let sanitized = sanitize_schema(&schema);
        let desc_type = sanitized["properties"]["description"]["type"]
            .as_str()
            .unwrap();
        assert_eq!(
            desc_type, "string",
            "Array type should be flattened to first non-null type"
        );
    }

    #[test]
    fn test_sanitize_schema_anyof() {
        // anyOf pattern for nullable types
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [
                        {"type": "string", "description": "A string value"},
                        {"type": "null"}
                    ]
                }
            }
        });

        let sanitized = sanitize_schema(&schema);
        let value_type = sanitized["properties"]["value"]["type"].as_str().unwrap();
        assert_eq!(
            value_type, "string",
            "anyOf should be flattened to first non-null variant"
        );
    }

    #[test]
    fn test_sanitize_schema_oneof() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "data": {
                    "oneOf": [
                        {"type": "integer"},
                        {"type": "null"}
                    ]
                }
            }
        });

        let sanitized = sanitize_schema(&schema);
        let data_type = sanitized["properties"]["data"]["type"].as_str().unwrap();
        assert_eq!(
            data_type, "integer",
            "oneOf should be flattened to first non-null variant"
        );
    }
}
