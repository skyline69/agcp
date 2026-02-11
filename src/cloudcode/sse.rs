use crate::format::google::{CloudCodeResponse, GenerateContentResponse, Part};
use crate::format::{
    ContentBlock, ContentDelta, ErrorData, MIN_SIGNATURE_LENGTH, MessageDeltaData,
    MessageDeltaUsage, MessageStart, ModelFamily, Role, StreamEvent, Usage,
    cache_thinking_signature, cache_tool_signature,
};
use crate::models::get_model_family;

pub struct SseParser {
    buffer: String,
    model: String,
    message_id: String,
    has_emitted_start: bool,
    block_index: u32,
    current_block_type: Option<BlockType>,
    current_thinking_signature: String,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    stop_reason: Option<String>,
    last_raw_data: String,
}

#[derive(Clone, Copy, PartialEq)]
enum BlockType {
    Text,
    Thinking,
    ToolUse,
}

impl SseParser {
    pub fn new(model: &str) -> Self {
        Self {
            buffer: String::with_capacity(4096),
            model: model.to_string(),
            message_id: format!("msg_{:032x}", generate_random()),
            has_emitted_start: false,
            block_index: 0,
            current_block_type: None,
            current_thinking_signature: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            stop_reason: None,
            last_raw_data: String::new(),
        }
    }

    /// Feed data to the parser and get any complete events
    pub fn feed(&mut self, data: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(data);

        let mut events = Vec::new();

        // Process complete events (handle both \n\n and \r\n\r\n as delimiters)
        loop {
            // Try to find the next event boundary
            let boundary = self
                .buffer
                .find("\r\n\r\n")
                .map(|p| (p, 4)) // CRLF version: skip 4 chars
                .or_else(|| self.buffer.find("\n\n").map(|p| (p, 2))); // LF version: skip 2 chars

            match boundary {
                Some((pos, skip)) => {
                    let line = self.buffer[..pos].to_string();
                    self.buffer.drain(..pos + skip);

                    if let Some(event) = self.parse_line(&line) {
                        events.extend(event);
                    }
                }
                None => break,
            }
        }

        events
    }

    /// Parse a single SSE line
    fn parse_line(&mut self, line: &str) -> Option<Vec<StreamEvent>> {
        // Handle data: prefix using strip_prefix
        let data = if let Some(stripped) = line.strip_prefix("data: ") {
            stripped
        } else if let Some(stripped) = line.strip_prefix("data:") {
            stripped
        } else {
            return None;
        };

        let data = data.trim();

        // Handle [DONE]
        if data == "[DONE]" {
            return Some(vec![create_message_stop()]);
        }

        // Store raw data for diagnostic logging
        self.last_raw_data.clear();
        self.last_raw_data
            .push_str(&data.chars().take(500).collect::<String>());

        // Parse JSON - try CloudCodeResponse wrapper first, then direct GenerateContentResponse
        let response: GenerateContentResponse =
            match serde_json::from_str::<CloudCodeResponse>(data) {
                Ok(wrapper) => wrapper.response,
                Err(wrapper_err) => {
                    // Before falling through, check if the JSON has a "response" key.
                    // If it does, this IS a CloudCodeResponse wrapper but with unexpected
                    // structure (e.g. missing "role" on content). Falling through to parse
                    // as bare GenerateContentResponse would silently produce all-None fields
                    // since serde ignores unknown keys.
                    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(data) {
                        if raw.get("response").is_some() {
                            // Extract any text from the response for a useful error message
                            let text = raw
                                .pointer("/response/candidates/0/content/parts/0/text")
                                .and_then(|v| v.as_str());

                            let message = if let Some(msg) = text {
                                msg.to_string()
                            } else {
                                format!(
                                    "Failed to parse CloudCodeResponse ({}). Raw: {}",
                                    wrapper_err,
                                    data.chars().take(300).collect::<String>()
                                )
                            };

                            tracing::warn!(
                                model = %self.model,
                                message = %message,
                                "Unparseable CloudCodeResponse wrapper"
                            );

                            return Some(vec![StreamEvent::Error {
                                error: ErrorData {
                                    error_type: "api_error".to_string(),
                                    message,
                                },
                            }]);
                        }

                        // Check for top-level error responses from Google API
                        // e.g. {"error": {"code": 404, "message": "...", "status": "NOT_FOUND"}}
                        if let Some(error_obj) = raw.get("error") {
                            let code = error_obj.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                            let message = error_obj
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("Unknown error");
                            let status = error_obj
                                .get("status")
                                .and_then(|s| s.as_str())
                                .unwrap_or("UNKNOWN");

                            tracing::warn!(
                                model = %self.model,
                                code = code,
                                status = %status,
                                message = %message,
                                "Google API error in SSE stream"
                            );

                            return Some(vec![StreamEvent::Error {
                                error: ErrorData {
                                    error_type: "api_error".to_string(),
                                    message: format!("Google API error ({}): {}", status, message),
                                },
                            }]);
                        }
                    }

                    // Try direct GenerateContentResponse parse (for non-wrapper responses)
                    match serde_json::from_str(data) {
                        Ok(r) => r,
                        Err(_) => {
                            tracing::debug!(
                                data = %data.chars().take(200).collect::<String>(),
                                "Failed to parse SSE data"
                            );
                            return None;
                        }
                    }
                }
            };

        // Check for error in the parsed response
        if let Some(error) = &response.error {
            tracing::warn!(
                model = %self.model,
                code = error.code,
                status = %error.status,
                message = %error.message,
                "Google API error in GenerateContentResponse"
            );

            return Some(vec![StreamEvent::Error {
                error: ErrorData {
                    error_type: "api_error".to_string(),
                    message: format!("Google API error ({}): {}", error.status, error.message),
                },
            }]);
        }

        // Convert to Anthropic events
        Some(self.process_response(&response))
    }

    /// Process a Google response and emit Anthropic events
    fn process_response(&mut self, response: &GenerateContentResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        // Extract usage metadata (including cache tokens)
        if let Some(usage) = &response.usage_metadata {
            self.input_tokens = usage.prompt_token_count;
            self.output_tokens = usage.candidates_token_count;
            self.cache_read_tokens = usage.cached_content_token_count;
        }

        let candidates = response.candidates.as_ref();
        let first_candidate = candidates.and_then(|c| c.first());

        // Check for blocked responses (finish_reason indicates blocked/filtered)
        if let Some(candidate) = first_candidate
            && let Some(reason) = &candidate.finish_reason
        {
            let reason_upper = reason.to_uppercase();
            if reason_upper == "SAFETY"
                || reason_upper == "BLOCKED"
                || reason_upper == "RECITATION"
                || reason_upper == "OTHER"
            {
                tracing::warn!(
                    model = %self.model,
                    finish_reason = %reason,
                    "Response blocked by Google API"
                );
                return vec![StreamEvent::Error {
                    error: ErrorData {
                        error_type: "api_error".to_string(),
                        message: format!("Response blocked by Google API (reason: {})", reason),
                    },
                }];
            }
        }

        // Check for prompt-level blocking (promptFeedback with blockReason)
        if let Some(feedback) = &response.prompt_feedback
            && let Some(reason) = &feedback.block_reason
        {
            tracing::warn!(
                model = %self.model,
                block_reason = %reason,
                "Prompt blocked by Google API"
            );
            return vec![StreamEvent::Error {
                error: ErrorData {
                    error_type: "invalid_request_error".to_string(),
                    message: format!("Prompt blocked by Google API (reason: {})", reason),
                },
            }];
        }

        // Check for no candidates at all (model unavailable or empty response)
        if first_candidate.is_none() && !self.has_emitted_start {
            tracing::warn!(
                model = %self.model,
                raw_data = %self.last_raw_data,
                "Google API returned response with no candidates"
            );
            return vec![StreamEvent::Error {
                error: ErrorData {
                    error_type: "api_error".to_string(),
                    message: format!(
                        "Model {} returned no candidates. The model may be unavailable.",
                        self.model
                    ),
                },
            }];
        }

        let content = first_candidate.and_then(|c| c.content.as_ref());
        let parts = content.map(|c| c.parts.as_slice()).unwrap_or(&[]);

        // Emit message_start on first data
        if !self.has_emitted_start && !parts.is_empty() {
            self.has_emitted_start = true;
            // Calculate input_tokens = promptTokenCount - cachedContentTokenCount
            let adjusted_input = self.input_tokens.saturating_sub(self.cache_read_tokens);
            events.push(StreamEvent::MessageStart {
                message: Box::new(MessageStart {
                    id: self.message_id.clone(),
                    message_type: "message".to_string(),
                    role: Role::Assistant,
                    content: vec![],
                    model: self.model.clone(),
                    stop_reason: None,
                    stop_sequence: None,
                    usage: Usage {
                        input_tokens: adjusted_input,
                        output_tokens: 0,
                        cache_read_input_tokens: if self.cache_read_tokens > 0 {
                            Some(self.cache_read_tokens)
                        } else {
                            None
                        },
                        cache_creation_input_tokens: Some(0),
                    },
                }),
            });
        }

        // Process each part
        for part in parts {
            match part {
                Part::Thought(thought) => {
                    let text = &thought.text;
                    let signature = thought.thought_signature.as_deref().unwrap_or("");

                    if self.current_block_type != Some(BlockType::Thinking) {
                        // Close previous block if any
                        if let Some(prev_type) = self.current_block_type {
                            events.extend(self.close_block(prev_type));
                        }
                        self.current_block_type = Some(BlockType::Thinking);
                        self.current_thinking_signature.clear();

                        events.push(StreamEvent::ContentBlockStart {
                            index: self.block_index,
                            content_block: ContentBlock::Thinking {
                                thinking: String::new(),
                                signature: None,
                            },
                        });
                    }

                    // Cache signature if present and long enough
                    if signature.len() >= MIN_SIGNATURE_LENGTH {
                        self.current_thinking_signature = signature.to_string();
                        // Cache with model family for cross-model compatibility
                        let family = ModelFamily::from_str(get_model_family(&self.model))
                            .unwrap_or(ModelFamily::Claude);
                        cache_thinking_signature(signature, family);
                    }

                    // Emit thinking delta
                    if !text.is_empty() {
                        events.push(StreamEvent::ContentBlockDelta {
                            index: self.block_index,
                            delta: ContentDelta::Thinking {
                                thinking: text.clone(),
                            },
                        });
                    }
                }

                Part::Text(text_part) => {
                    // Skip empty text parts
                    if text_part.text.is_empty() {
                        continue;
                    }

                    if self.current_block_type != Some(BlockType::Text) {
                        // If switching from thinking, emit signature first
                        if self.current_block_type == Some(BlockType::Thinking)
                            && !self.current_thinking_signature.is_empty()
                        {
                            events.push(StreamEvent::ContentBlockDelta {
                                index: self.block_index,
                                delta: ContentDelta::Signature {
                                    signature: self.current_thinking_signature.clone(),
                                },
                            });
                            self.current_thinking_signature.clear();
                        }
                        // Close previous block if any
                        if let Some(prev_type) = self.current_block_type {
                            events.extend(self.close_block(prev_type));
                        }
                        self.current_block_type = Some(BlockType::Text);

                        events.push(StreamEvent::ContentBlockStart {
                            index: self.block_index,
                            content_block: ContentBlock::Text {
                                text: String::new(),
                                cache_control: None,
                            },
                        });
                    }

                    events.push(StreamEvent::ContentBlockDelta {
                        index: self.block_index,
                        delta: ContentDelta::Text {
                            text: text_part.text.clone(),
                        },
                    });
                }

                Part::FunctionCall(fc) => {
                    // Get signature from function call part
                    let function_call_signature = fc.thought_signature.as_deref().unwrap_or("");

                    // If switching from thinking, emit signature first
                    if self.current_block_type == Some(BlockType::Thinking)
                        && !self.current_thinking_signature.is_empty()
                    {
                        events.push(StreamEvent::ContentBlockDelta {
                            index: self.block_index,
                            delta: ContentDelta::Signature {
                                signature: self.current_thinking_signature.clone(),
                            },
                        });
                        self.current_thinking_signature.clear();
                    }
                    // Close previous block if any
                    if let Some(prev_type) = self.current_block_type {
                        events.extend(self.close_block(prev_type));
                    }
                    self.current_block_type = Some(BlockType::ToolUse);
                    self.stop_reason = Some("tool_use".to_string());

                    let tool_id = fc
                        .function_call
                        .args
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("toolu_{:024x}", generate_random()));

                    events.push(StreamEvent::ContentBlockStart {
                        index: self.block_index,
                        content_block: ContentBlock::ToolUse {
                            id: tool_id.clone(),
                            name: fc.function_call.name.clone(),
                            input: serde_json::Value::Object(serde_json::Map::new()),
                        },
                    });

                    // Emit input as JSON delta (strip 'id' field if present as it's not a function parameter)
                    let args_to_send =
                        if let serde_json::Value::Object(mut obj) = fc.function_call.args.clone() {
                            obj.remove("id");
                            serde_json::Value::Object(obj)
                        } else {
                            fc.function_call.args.clone()
                        };
                    let args_json = serde_json::to_string(&args_to_send).unwrap_or_default();
                    events.push(StreamEvent::ContentBlockDelta {
                        index: self.block_index,
                        delta: ContentDelta::InputJson {
                            partial_json: args_json,
                        },
                    });

                    // Cache signature for tool ID (for later restoration when Claude Code strips it)
                    if function_call_signature.len() >= MIN_SIGNATURE_LENGTH {
                        cache_tool_signature(&tool_id, function_call_signature);
                    }
                }

                _ => {}
            }
        }

        // Check finish reason (only if not already set by tool_use)
        if let Some(candidate) = first_candidate
            && let Some(finish_reason) = &candidate.finish_reason
            && self.stop_reason.is_none()
        {
            self.stop_reason = Some(match finish_reason.as_str() {
                "MAX_TOKENS" => "max_tokens".to_string(),
                "STOP" => "end_turn".to_string(),
                _ => "end_turn".to_string(),
            });
        }

        events
    }

    /// Close the current block and increment index
    fn close_block(&mut self, _block_type: BlockType) -> Vec<StreamEvent> {
        let events = vec![StreamEvent::ContentBlockStop {
            index: self.block_index,
        }];
        self.block_index += 1;
        events
    }

    /// Finish parsing and get final events
    pub fn finish(self) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        // Close any open block
        if let Some(block_type) = self.current_block_type {
            // If in thinking block, emit signature first
            if block_type == BlockType::Thinking && !self.current_thinking_signature.is_empty() {
                events.push(StreamEvent::ContentBlockDelta {
                    index: self.block_index,
                    delta: ContentDelta::Signature {
                        signature: self.current_thinking_signature.clone(),
                    },
                });
            }
            events.push(StreamEvent::ContentBlockStop {
                index: self.block_index,
            });
        }

        // Emit message_delta
        let stop_reason = self.stop_reason.as_deref().unwrap_or("end_turn");
        events.push(StreamEvent::MessageDelta {
            delta: MessageDeltaData {
                stop_reason: Some(match stop_reason {
                    "max_tokens" => crate::format::StopReason::MaxTokens,
                    "tool_use" => crate::format::StopReason::ToolUse,
                    _ => crate::format::StopReason::EndTurn,
                }),
                stop_sequence: None,
            },
            usage: MessageDeltaUsage {
                output_tokens: self.output_tokens,
            },
        });

        events
    }
}

fn generate_random() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub fn create_message_stop() -> StreamEvent {
    StreamEvent::MessageStop
}

pub fn format_sse_event(event: &StreamEvent) -> String {
    let event_type = match event {
        StreamEvent::MessageStart { .. } => "message_start",
        StreamEvent::ContentBlockStart { .. } => "content_block_start",
        StreamEvent::ContentBlockDelta { .. } => "content_block_delta",
        StreamEvent::ContentBlockStop { .. } => "content_block_stop",
        StreamEvent::MessageDelta { .. } => "message_delta",
        StreamEvent::MessageStop => "message_stop",
        StreamEvent::Ping => "ping",
        StreamEvent::Error { .. } => "error",
    };

    let data = serde_json::to_string(event).unwrap_or_default();

    format!("event: {}\ndata: {}\n\n", event_type, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_parser_simple_text() {
        let mut parser = SseParser::new("claude-sonnet-4-5");

        // Simulate a simple text response
        let data = r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello, world!"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"cachedContentTokenCount":0}}}

"#;

        let events = parser.feed(data);

        // Should have message_start and content events
        assert!(!events.is_empty());

        // First event should be message_start
        match &events[0] {
            StreamEvent::MessageStart { message } => {
                assert_eq!(message.model, "claude-sonnet-4-5");
                assert!(message.id.starts_with("msg_"));
            }
            _ => panic!("Expected MessageStart event"),
        }
    }

    #[test]
    fn test_sse_parser_done_signal() {
        let mut parser = SseParser::new("claude-sonnet-4-5");

        let events = parser.feed("data: [DONE]\n\n");

        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::MessageStop => {}
            _ => panic!("Expected MessageStop event"),
        }
    }

    #[test]
    fn test_sse_parser_finish() {
        let mut parser = SseParser::new("claude-sonnet-4-5");

        // Feed some text first
        let data = r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"cachedContentTokenCount":0}}}

"#;
        let _ = parser.feed(data);

        // Finish should emit final events
        let events = parser.finish();

        // Should have content_block_stop and message_delta
        let has_message_delta = events
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageDelta { .. }));
        assert!(has_message_delta);
    }

    #[test]
    fn test_format_sse_event() {
        let event = StreamEvent::MessageStop;
        let formatted = format_sse_event(&event);

        assert!(formatted.starts_with("event: message_stop\n"));
        assert!(formatted.ends_with("\n\n"));
    }

    #[test]
    fn test_create_message_stop() {
        let event = create_message_stop();
        match event {
            StreamEvent::MessageStop => {}
            _ => panic!("Expected MessageStop"),
        }
    }

    #[test]
    fn test_sse_parser_google_error_in_stream() {
        let mut parser = SseParser::new("claude-opus-4-5-thinking");

        // Simulate a Google API error response embedded in SSE stream
        let data = r#"data: {"error":{"code":404,"message":"Requested entity was not found.","status":"NOT_FOUND"}}

"#;

        let events = parser.feed(data);

        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { error } => {
                assert_eq!(error.error_type, "api_error");
                assert!(error.message.contains("NOT_FOUND"));
                assert!(error.message.contains("Requested entity was not found"));
            }
            _ => panic!("Expected Error event, got {:?}", events[0]),
        }
    }

    #[test]
    fn test_sse_parser_error_in_generate_content_response() {
        let mut parser = SseParser::new("claude-opus-4-5-thinking");

        // Simulate a Google API error within GenerateContentResponse wrapper
        let data = r#"data: {"candidates":null,"error":{"code":404,"message":"Model not available","status":"NOT_FOUND"},"usageMetadata":null}

"#;

        let events = parser.feed(data);

        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { error } => {
                assert_eq!(error.error_type, "api_error");
                assert!(error.message.contains("NOT_FOUND"));
                assert!(error.message.contains("Model not available"));
            }
            _ => panic!("Expected Error event, got {:?}", events[0]),
        }
    }

    #[test]
    fn test_sse_parser_cloudcode_wrapper_error() {
        let mut parser = SseParser::new("claude-opus-4-5-thinking");

        // Simulate an error within CloudCodeResponse wrapper
        let data = r#"data: {"response":{"candidates":null,"error":{"code":503,"message":"Model capacity exhausted","status":"UNAVAILABLE"},"usageMetadata":null}}

"#;

        let events = parser.feed(data);

        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { error } => {
                assert_eq!(error.error_type, "api_error");
                assert!(error.message.contains("UNAVAILABLE"));
                assert!(error.message.contains("Model capacity exhausted"));
            }
            _ => panic!("Expected Error event, got {:?}", events[0]),
        }
    }

    #[test]
    fn test_sse_parser_version_gate_response() {
        // Reproduce the exact response Google returns when client version is outdated.
        // The response has candidates with content but no "role" field on the content object,
        // causing CloudCodeResponse parsing to fail. We should extract the text and return
        // it as an error instead of silently misreporting "no candidates."
        let mut parser = SseParser::new("claude-opus-4-6-thinking");

        let data = "data: {\"response\": {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"This version of Antigravity is no longer supported. Please update to receive the latest features!\"}]}}]}}\n\n";

        let events = parser.feed(data);

        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { error } => {
                assert_eq!(error.error_type, "api_error");
                assert!(
                    error.message.contains("no longer supported"),
                    "Error message should contain the version gate text, got: {}",
                    error.message
                );
            }
            _ => panic!("Expected Error event, got {:?}", events[0]),
        }
    }
}
