use crate::format::anthropic::{
    ContentBlock, Message, MessageContent, MessagesRequest, MessagesResponse, Role, SystemPrompt,
    Tool, ToolResultContent,
};
use crate::format::openai::{
    ChatCompletionRequest, ChatCompletionResponse, ChatContent, ChatUsage, Choice, FunctionCall,
    ResponseMessage, StopSequence, ToolCall,
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Convert OpenAI ChatCompletionRequest to Anthropic MessagesRequest
pub fn openai_to_anthropic(request: &ChatCompletionRequest) -> MessagesRequest {
    let mut system: Option<SystemPrompt> = None;
    let mut messages: Vec<Message> = Vec::new();

    for msg in &request.messages {
        match msg.role.as_str() {
            "system" => {
                // Extract system message
                if let Some(content) = &msg.content {
                    let text = content_to_string(content);
                    system = Some(SystemPrompt::Text(text));
                }
            }
            "user" => {
                let content = msg
                    .content
                    .as_ref()
                    .map(convert_chat_content)
                    .unwrap_or_else(|| MessageContent::Text(String::new()));
                messages.push(Message {
                    role: Role::User,
                    content,
                });
            }
            "assistant" => {
                let mut blocks: Vec<ContentBlock> = Vec::new();

                // Add text content if present
                if let Some(content) = &msg.content {
                    let text = content_to_string(content);
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text,
                            cache_control: None,
                        });
                    }
                }

                // Add tool calls if present
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        blocks.push(ContentBlock::ToolUse {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            input,
                        });
                    }
                }

                if blocks.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: String::new(),
                        cache_control: None,
                    });
                }

                messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(blocks),
                });
            }
            "tool" => {
                // Tool result message
                if let Some(tool_call_id) = &msg.tool_call_id {
                    let text = msg
                        .content
                        .as_ref()
                        .map(content_to_string)
                        .unwrap_or_default();
                    let block = ContentBlock::ToolResult {
                        tool_use_id: tool_call_id.clone(),
                        content: ToolResultContent::Text(text),
                        is_error: None,
                    };

                    // Try to append to last user message, or create new one
                    if let Some(last) = messages.last_mut()
                        && matches!(last.role, Role::User)
                        && let MessageContent::Blocks(blocks) = &mut last.content
                    {
                        blocks.push(block);
                        continue;
                    }

                    // Create new user message with tool result
                    messages.push(Message {
                        role: Role::User,
                        content: MessageContent::Blocks(vec![block]),
                    });
                }
            }
            _ => {}
        }
    }

    // Determine max_tokens
    let max_tokens = request
        .max_completion_tokens
        .or(request.max_tokens)
        .unwrap_or(4096);

    // Convert stop sequences
    let stop_sequences = request.stop.as_ref().map(|s| match s {
        StopSequence::Single(s) => vec![s.clone()],
        StopSequence::Multiple(v) => v.clone(),
    });

    // Convert tools
    let tools = request.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|t| Tool {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                input_schema: t
                    .function
                    .parameters
                    .clone()
                    .unwrap_or(serde_json::json!({"type": "object", "properties": {}})),
            })
            .collect()
    });

    MessagesRequest {
        model: request.model.clone(),
        messages,
        max_tokens,
        system,
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: None,
        stop_sequences,
        stream: request.stream,
        tools,
    }
}

/// Convert Anthropic MessagesResponse to OpenAI ChatCompletionResponse
pub fn anthropic_to_openai(
    response: &MessagesResponse,
    model: &str,
    request_id: &str,
) -> ChatCompletionResponse {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Collect text content and tool calls
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in &response.content {
        match block {
            ContentBlock::Text { text, .. } => {
                text_parts.push(text.clone());
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: name.clone(),
                        arguments: serde_json::to_string(input).unwrap_or_default(),
                    },
                });
            }
            ContentBlock::Thinking { thinking, .. } => {
                // Include thinking as text with marker
                text_parts.push(format!("<thinking>\n{}\n</thinking>", thinking));
            }
            _ => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };

    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    let finish_reason = response.stop_reason.map(|r| r.to_openai_str().to_string());

    ChatCompletionResponse {
        id: format!("chatcmpl-{}", request_id),
        object: "chat.completion".to_string(),
        created,
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            message: ResponseMessage {
                role: "assistant".to_string(),
                content,
                tool_calls: tool_calls_opt,
                refusal: None,
            },
            finish_reason,
            logprobs: None,
        }],
        usage: Some(ChatUsage {
            prompt_tokens: response.usage.input_tokens,
            completion_tokens: response.usage.output_tokens,
            total_tokens: response.usage.input_tokens + response.usage.output_tokens,
        }),
        system_fingerprint: None,
    }
}

fn content_to_string(content: &ChatContent) -> String {
    match content {
        ChatContent::Text(s) => s.clone(),
        ChatContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                crate::format::openai::ChatContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn convert_chat_content(content: &ChatContent) -> MessageContent {
    match content {
        ChatContent::Text(s) => MessageContent::Text(s.clone()),
        ChatContent::Parts(parts) => {
            let blocks: Vec<ContentBlock> = parts
                .iter()
                .filter_map(|p| match p {
                    crate::format::openai::ChatContentPart::Text { text } => {
                        Some(ContentBlock::Text {
                            text: text.clone(),
                            cache_control: None,
                        })
                    }
                    crate::format::openai::ChatContentPart::ImageUrl { image_url } => {
                        // Try to parse data URL
                        if let Some(data) = parse_data_url(&image_url.url) {
                            Some(ContentBlock::Image {
                                source: crate::format::anthropic::ImageSource {
                                    source_type: "base64".to_string(),
                                    media_type: data.0,
                                    data: data.1,
                                },
                            })
                        } else {
                            None
                        }
                    }
                })
                .collect();
            MessageContent::Blocks(blocks)
        }
    }
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((mime_part, data)) = rest.split_once(",")
    {
        let mime_type = mime_part
            .split(';')
            .next()
            .unwrap_or("image/png")
            .to_string();
        return Some((mime_type, data.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::anthropic::StopReason;
    use crate::format::openai::ChatMessage;

    #[test]
    fn test_simple_chat_to_anthropic() {
        let request = ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: Some(ChatContent::Text("You are helpful".to_string())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: Some(ChatContent::Text("Hello".to_string())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            max_tokens: Some(100),
            max_completion_tokens: None,
            temperature: Some(0.7),
            top_p: None,
            stop: None,
            stream: false,
            tools: None,
            tool_choice: None,
            n: None,
            user: None,
        };

        let anthropic = openai_to_anthropic(&request);

        assert_eq!(anthropic.model, "gpt-4");
        assert_eq!(anthropic.max_tokens, 100);
        assert!(anthropic.system.is_some());
        assert_eq!(anthropic.messages.len(), 1);
        assert!(matches!(anthropic.messages[0].role, Role::User));
    }

    #[test]
    fn test_anthropic_to_openai_simple() {
        let response = MessagesResponse {
            id: "msg_123".to_string(),
            response_type: "message".to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Hello!".to_string(),
                cache_control: None,
            }],
            model: "claude-sonnet-4-5".to_string(),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            usage: crate::format::anthropic::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        };

        let openai = anthropic_to_openai(&response, "claude-sonnet-4-5", "req_123");

        assert!(openai.id.starts_with("chatcmpl-"));
        assert_eq!(openai.object, "chat.completion");
        assert_eq!(openai.choices.len(), 1);
        assert_eq!(
            openai.choices[0].message.content,
            Some("Hello!".to_string())
        );
        assert_eq!(openai.choices[0].finish_reason, Some("stop".to_string()));
        assert!(openai.usage.is_some());
    }

    #[test]
    fn test_tool_call_conversion() {
        let response = MessagesResponse {
            id: "msg_123".to_string(),
            response_type: "message".to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_123".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"location": "NYC"}),
            }],
            model: "claude-sonnet-4-5".to_string(),
            stop_reason: Some(StopReason::ToolUse),
            stop_sequence: None,
            usage: crate::format::anthropic::Usage::default(),
        };

        let openai = anthropic_to_openai(&response, "test", "req_1");

        assert_eq!(
            openai.choices[0].finish_reason,
            Some("tool_calls".to_string())
        );
        let tool_calls = openai.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "get_weather");
    }
}
