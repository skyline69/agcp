//! Conversion between Responses API and Anthropic Messages API.

use crate::format::anthropic::{
    ContentBlock, Message, MessageContent, MessagesRequest, MessagesResponse, Role, SystemPrompt,
    Tool,
};
use crate::format::responses::{
    InputTokensDetails, ResponseInput, ResponseInputContent, ResponseInputItem, ResponseInputPart,
    ResponseOutputContent, ResponseOutputItem, ResponseUsage, ResponsesRequest, ResponsesResponse,
};

/// Convert a Responses API request to an Anthropic Messages API request
pub fn responses_to_anthropic(request: &ResponsesRequest) -> MessagesRequest {
    let mut messages = Vec::new();

    // Convert input to messages
    if let Some(input) = &request.input {
        match input {
            ResponseInput::Text(text) => {
                messages.push(Message {
                    role: Role::User,
                    content: MessageContent::Text(text.clone()),
                });
            }
            ResponseInput::Items(items) => {
                for item in items {
                    if let ResponseInputItem::Message { role, content } = item {
                        let anthropic_role = match role.as_str() {
                            "user" => Role::User,
                            "assistant" => Role::Assistant,
                            _ => Role::User,
                        };

                        let text = match content {
                            ResponseInputContent::Text(t) => t.clone(),
                            ResponseInputContent::Parts(parts) => {
                                let mut text = String::new();
                                for part in parts {
                                    match part {
                                        ResponseInputPart::InputText { text: t } => {
                                            text.push_str(t);
                                        }
                                        ResponseInputPart::OutputText { text: t } => {
                                            text.push_str(t);
                                        }
                                        ResponseInputPart::Other => {}
                                    }
                                }
                                text
                            }
                        };

                        messages.push(Message {
                            role: anthropic_role,
                            content: MessageContent::Text(text),
                        });
                    }
                }
            }
        }
    }

    // If no messages, add a placeholder
    if messages.is_empty() {
        messages.push(Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        });
    }

    // Convert tools
    let tools = request.tools.as_ref().map(|tools| {
        tools
            .iter()
            .filter_map(|t| {
                if t.tool_type == "function" {
                    Some(Tool {
                        name: t.name.clone().unwrap_or_default(),
                        description: t.description.clone(),
                        input_schema: t.parameters.clone().unwrap_or(serde_json::json!({
                            "type": "object",
                            "properties": {}
                        })),
                    })
                } else {
                    None
                }
            })
            .collect()
    });

    // Handle reasoning/thinking configuration
    let model = request
        .model
        .clone()
        .unwrap_or_else(|| "claude-sonnet-4-5".to_string());

    MessagesRequest {
        model,
        messages,
        max_tokens: request.max_output_tokens.unwrap_or(16384),
        system: request
            .instructions
            .as_ref()
            .map(|i| SystemPrompt::Text(i.clone())),
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: None,
        stop_sequences: None,
        stream: request.stream,
        tools,
        tool_choice: None,
    }
}

/// Convert an Anthropic Messages API response to a Responses API response
pub fn anthropic_to_responses(
    response: &MessagesResponse,
    model: &str,
    request_id: &str,
) -> ResponsesResponse {
    let mut output = Vec::new();
    let mut reasoning_text = String::new();

    // Process content blocks
    let mut message_content = Vec::new();

    for block in &response.content {
        match block {
            ContentBlock::Text { text, .. } => {
                message_content.push(ResponseOutputContent::OutputText {
                    text: text.clone(),
                    annotations: vec![],
                });
            }
            ContentBlock::Thinking { thinking, .. } => {
                reasoning_text.push_str(thinking);
            }
            ContentBlock::ToolUse { id, name, input } => {
                // Add function call output item
                output.push(ResponseOutputItem::FunctionCall {
                    id: format!("fc_{}", id),
                    call_id: id.clone(),
                    name: name.clone(),
                    arguments: serde_json::to_string(input).unwrap_or_default(),
                    status: "completed",
                });
            }
            _ => {}
        }
    }

    // Add reasoning item if present
    if !reasoning_text.is_empty() {
        output.push(ResponseOutputItem::Reasoning {
            id: format!("rs_{}", &request_id[..8.min(request_id.len())]),
            status: "completed",
            summary: Some(vec![ResponseOutputContent::OutputText {
                text: reasoning_text,
                annotations: vec![],
            }]),
        });
    }

    // Add message output item
    if !message_content.is_empty() {
        output.push(ResponseOutputItem::Message {
            id: format!("msg_{}", &request_id[..8.min(request_id.len())]),
            role: "assistant",
            status: "completed",
            content: message_content,
        });
    }

    // Build usage
    let u = &response.usage;
    let usage = Some(ResponseUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        total_tokens: u.input_tokens + u.output_tokens,
        input_tokens_details: u.cache_read_input_tokens.map(|cached| InputTokensDetails {
            cached_tokens: cached,
        }),
        output_tokens_details: None,
    });

    ResponsesResponse {
        id: format!("resp_{}", request_id),
        object: "response",
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        model: model.to_string(),
        output,
        parallel_tool_calls: true,
        tool_choice: "auto",
        tools: vec![],
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        usage,
        status: "completed",
    }
}
