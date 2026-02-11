use crate::format::anthropic::{
    ContentBlock, ContentDelta, MessagesResponse, Role, StopReason, StreamEvent, Usage,
};
use crate::format::google::{Candidate, GenerateContentResponse, Part, UsageMetadata};
use crate::format::signature_cache::{
    MIN_SIGNATURE_LENGTH, ModelFamily, cache_thinking_signature, cache_tool_signature,
};
use crate::models::get_model_family;

pub fn convert_response(
    response: &GenerateContentResponse,
    model: &str,
    request_id: &str,
) -> MessagesResponse {
    let model_family =
        ModelFamily::from_str(get_model_family(model)).unwrap_or(ModelFamily::Claude);

    let (content, stop_reason) = match response.candidates.as_ref().and_then(|c| c.first()) {
        Some(candidate) => convert_candidate(candidate, model_family),
        None => (vec![], None),
    };

    let usage = response
        .usage_metadata
        .as_ref()
        .map(convert_usage)
        .unwrap_or_default();

    MessagesResponse {
        id: request_id.to_string(),
        response_type: "message".to_string(),
        role: Role::Assistant,
        content,
        model: model.to_string(),
        stop_reason,
        stop_sequence: None,
        usage,
    }
}

fn convert_candidate(
    candidate: &Candidate,
    model_family: ModelFamily,
) -> (Vec<ContentBlock>, Option<StopReason>) {
    let content = candidate
        .content
        .as_ref()
        .map(|c| convert_parts(&c.parts, model_family))
        .unwrap_or_default();

    let stop_reason = candidate
        .finish_reason
        .as_ref()
        .map(|r| convert_finish_reason(r));

    (content, stop_reason)
}

fn convert_parts(parts: &[Part], model_family: ModelFamily) -> Vec<ContentBlock> {
    parts
        .iter()
        .filter_map(|p| convert_part(p, model_family))
        .collect()
}

fn convert_part(part: &Part, model_family: ModelFamily) -> Option<ContentBlock> {
    match part {
        Part::Text(text_part) => Some(ContentBlock::Text {
            text: text_part.text.clone(),
            cache_control: None,
        }),
        Part::FunctionCall(fc) => {
            let id = fc
                .function_call
                .id
                .clone()
                .unwrap_or_else(|| format!("toolu_{}", generate_id()));

            // Cache signature for tool ID if present
            if let Some(sig) = &fc.thought_signature
                && sig.len() >= MIN_SIGNATURE_LENGTH
            {
                cache_tool_signature(&id, sig);
            }

            Some(ContentBlock::ToolUse {
                id,
                name: fc.function_call.name.clone(),
                input: fc.function_call.args.clone(),
            })
        }
        Part::Thought(thought) => {
            let signature = thought.thought_signature.clone();

            // Cache thinking signature with model family
            if let Some(ref sig) = signature
                && sig.len() >= MIN_SIGNATURE_LENGTH
            {
                cache_thinking_signature(sig, model_family);
            }

            Some(ContentBlock::Thinking {
                thinking: thought.text.clone(),
                signature,
            })
        }
        Part::InlineData(_) | Part::FunctionResponse(_) => None,
    }
}

fn convert_finish_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "STOP_SEQUENCE" => StopReason::StopSequence,
        "TOOL_CALL" | "FUNCTION_CALL" => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    }
}

fn convert_usage(usage: &UsageMetadata) -> Usage {
    let input_tokens = if usage.cached_content_token_count > 0 {
        usage
            .prompt_token_count
            .saturating_sub(usage.cached_content_token_count)
    } else {
        usage.prompt_token_count
    };

    Usage {
        input_tokens,
        output_tokens: usage.candidates_token_count,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: if usage.cached_content_token_count > 0 {
            Some(usage.cached_content_token_count)
        } else {
            None
        },
    }
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}", nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::google::{Content, TextPart};

    fn create_test_response(text: &str, finish_reason: Option<&str>) -> GenerateContentResponse {
        GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Some(Content {
                    role: "model".to_string(),
                    parts: vec![Part::Text(TextPart {
                        text: text.to_string(),
                    })],
                }),
                finish_reason: finish_reason.map(String::from),
                safety_ratings: None,
            }]),
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 100,
                candidates_token_count: 50,
                total_token_count: 150,
                cached_content_token_count: 0,
            }),
            error: None,
            prompt_feedback: None,
        }
    }

    #[test]
    fn test_convert_simple_response() {
        let response = create_test_response("Hello, world!", Some("STOP"));
        let result = convert_response(&response, "claude-sonnet-4-5", "req_123");

        assert_eq!(result.id, "req_123");
        assert_eq!(result.model, "claude-sonnet-4-5");
        assert_eq!(result.role, Role::Assistant);
        assert_eq!(result.content.len(), 1);

        match &result.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "Hello, world!"),
            _ => panic!("Expected Text block"),
        }
    }

    #[test]
    fn test_convert_stop_reason() {
        let response = create_test_response("Text", Some("STOP"));
        let result = convert_response(&response, "test", "req_1");
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));

        let response = create_test_response("Text", Some("MAX_TOKENS"));
        let result = convert_response(&response, "test", "req_2");
        assert_eq!(result.stop_reason, Some(StopReason::MaxTokens));

        let response = create_test_response("Text", Some("TOOL_CALL"));
        let result = convert_response(&response, "test", "req_3");
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn test_convert_usage_with_cache() {
        let response = GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Some(Content {
                    role: "model".to_string(),
                    parts: vec![Part::Text(TextPart {
                        text: "Hi".to_string(),
                    })],
                }),
                finish_reason: Some("STOP".to_string()),
                safety_ratings: None,
            }]),
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 1000,
                candidates_token_count: 100,
                total_token_count: 1100,
                cached_content_token_count: 800,
            }),
            error: None,
            prompt_feedback: None,
        };

        let result = convert_response(&response, "test", "req_cache");

        // input_tokens should be prompt - cached
        assert_eq!(result.usage.input_tokens, 200);
        assert_eq!(result.usage.output_tokens, 100);
        assert_eq!(result.usage.cache_read_input_tokens, Some(800));
    }

    #[test]
    fn test_convert_empty_response() {
        let response = GenerateContentResponse {
            candidates: None,
            usage_metadata: None,
            error: None,
            prompt_feedback: None,
        };

        let result = convert_response(&response, "test", "req_empty");

        assert!(result.content.is_empty());
        assert_eq!(result.stop_reason, None);
    }

    #[test]
    fn test_build_response_from_events_text() {
        let events = vec![
            StreamEvent::MessageStart {
                message: Box::new(crate::format::MessageStart {
                    id: "msg_123".to_string(),
                    message_type: "message".to_string(),
                    role: Role::Assistant,
                    content: vec![],
                    model: "claude-sonnet-4-5".to_string(),
                    stop_reason: None,
                    stop_sequence: None,
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 0,
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                    },
                }),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::Text {
                    text: "Hello ".to_string(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::Text {
                    text: "world!".to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: crate::format::MessageDeltaData {
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: crate::format::MessageDeltaUsage { output_tokens: 10 },
            },
        ];

        let result = build_response_from_events(&events, "claude-sonnet-4-5", "req_stream");

        assert_eq!(result.id, "req_stream");
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "Hello world!"),
            _ => panic!("Expected Text block"),
        }
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(result.usage.output_tokens, 10);
    }
}

// Thinking models must use streaming endpoint but client may want non-streaming response
pub fn build_response_from_events(
    events: &[StreamEvent],
    model: &str,
    request_id: &str,
) -> MessagesResponse {
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut stop_reason: Option<StopReason> = None;
    let mut usage = Usage::default();

    let mut current_text = String::new();
    let mut current_thinking = String::new();
    let mut current_signature = String::new();
    let mut in_text_block = false;
    let mut in_thinking_block = false;

    for event in events {
        match event {
            StreamEvent::MessageStart { message } => {
                usage = message.usage.clone();
            }
            StreamEvent::ContentBlockStart { content_block, .. } => {
                // Start tracking this block type
                match content_block {
                    ContentBlock::Text { .. } => {
                        in_text_block = true;
                        current_text.clear();
                    }
                    ContentBlock::Thinking { signature, .. } => {
                        in_thinking_block = true;
                        current_thinking.clear();
                        current_signature = signature.clone().unwrap_or_default();
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        // Tool use blocks come complete
                        content.push(ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        });
                    }
                    _ => {}
                }
            }
            StreamEvent::ContentBlockDelta { delta, .. } => {
                match delta {
                    ContentDelta::Text { text } => {
                        if in_text_block {
                            current_text.push_str(text);
                        }
                    }
                    ContentDelta::Thinking { thinking } => {
                        if in_thinking_block {
                            current_thinking.push_str(thinking);
                        }
                    }
                    ContentDelta::InputJson { partial_json } => {
                        // Tool input - update last tool_use block
                        if let Some(ContentBlock::ToolUse { input, .. }) = content.last_mut()
                            && let Ok(parsed) = serde_json::from_str(partial_json)
                        {
                            *input = parsed;
                        }
                    }
                    ContentDelta::Signature { signature } => {
                        // Signature for thinking block
                        if in_thinking_block {
                            current_signature = signature.clone();
                        }
                    }
                }
            }
            StreamEvent::ContentBlockStop { .. } => {
                // Finalize the current block
                if in_text_block && !current_text.is_empty() {
                    content.push(ContentBlock::Text {
                        text: std::mem::take(&mut current_text),
                        cache_control: None,
                    });
                    in_text_block = false;
                }
                if in_thinking_block && !current_thinking.is_empty() {
                    let signature = if current_signature.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut current_signature))
                    };
                    content.push(ContentBlock::Thinking {
                        thinking: std::mem::take(&mut current_thinking),
                        signature,
                    });
                    in_thinking_block = false;
                }
            }
            StreamEvent::MessageDelta {
                delta,
                usage: delta_usage,
            } => {
                if delta.stop_reason.is_some() {
                    stop_reason = delta.stop_reason;
                }
                usage.output_tokens = delta_usage.output_tokens;
            }
            _ => {}
        }
    }

    // Finalize any remaining blocks
    if in_text_block && !current_text.is_empty() {
        content.push(ContentBlock::Text {
            text: current_text,
            cache_control: None,
        });
    }
    if in_thinking_block && !current_thinking.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: current_thinking,
            signature: if current_signature.is_empty() {
                None
            } else {
                Some(current_signature)
            },
        });
    }

    MessagesResponse {
        id: request_id.to_string(),
        response_type: "message".to_string(),
        role: Role::Assistant,
        content,
        model: model.to_string(),
        stop_reason,
        stop_sequence: None,
        usage,
    }
}
