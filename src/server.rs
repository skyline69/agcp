use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, trace, warn};

use crate::auth::HttpClient;
use crate::auth::accounts::AccountStore;
use crate::cache::ResponseCache;
use crate::cloudcode::{
    CloudCodeClient, SseParser, build_request, create_message_stop, fetch_model_quotas,
    format_sse_event, parse_response,
};
use crate::config::get_config;
use crate::error::{ApiError, AuthError, Error};
use crate::format::{
    ChatCompletionRequest, MessagesRequest, ModelInfo, ModelsResponse, StreamEvent,
};
use crate::models::{Model, get_fallback_model, is_thinking_model, resolve_model_alias};
use crate::stats::get_stats;

/// Maximum request body size (10 MB).
const MAX_REQUEST_SIZE: usize = 10 * 1024 * 1024;

/// Shared server state passed to all request handlers.
///
/// Contains:
/// - `accounts`: OAuth account store with token management
/// - `http_client`: Shared HTTP client for OAuth operations
/// - `cloudcode_client`: Google Cloud Code API client
/// - `cache`: LRU response cache for non-streaming requests
pub struct ServerState {
    pub accounts: RwLock<AccountStore>,
    pub http_client: HttpClient,
    pub cloudcode_client: CloudCodeClient,
    pub cache: Mutex<ResponseCache>,
}

/// Handle an incoming TCP connection.
///
/// Upgrades the connection to HTTP/1.1 and routes requests to the appropriate handler.
pub async fn handle_connection(
    stream: TcpStream,
    remote_addr: SocketAddr,
    state: Arc<ServerState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let io = TokioIo::new(stream);

    let service = service_fn(move |req| {
        let state = state.clone();
        let remote = remote_addr;
        async move { handle_request(req, state, remote).await }
    });

    http1::Builder::new()
        .keep_alive(true)
        .serve_connection(io, service)
        .await?;

    Ok(())
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    remote_addr: SocketAddr,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let request_id = generate_request_id();

    debug!(
        method = %method,
        path = %path,
        remote = %remote_addr,
        request_id = %request_id,
        "Received request"
    );

    let start = std::time::Instant::now();

    // Check API key authentication for /v1/* endpoints
    let config = get_config();
    if path.starts_with("/v1/")
        && let Some(ref expected_key) = config.server.api_key
    {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        let x_api_key = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());

        let provided_key = auth_header
            .and_then(|h| h.strip_prefix("Bearer "))
            .or(x_api_key);

        if provided_key != Some(expected_key.as_str()) {
            warn!(
                remote = %remote_addr,
                request_id = %request_id,
                "Unauthorized request - invalid API key"
            );
            return Ok(json_response(
                StatusCode::UNAUTHORIZED,
                r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid or missing API key"}}"#,
            ));
        }
    }

    let request_timeout = Duration::from_secs(config.server.request_timeout_secs);
    let response = match tokio::time::timeout(request_timeout, async {
        match (method.clone(), path.as_str()) {
            // Messages API (with and without /v1 prefix)
            (Method::POST, "/v1/messages") | (Method::POST, "/messages") => {
                handle_messages(req, state, &request_id).await
            }

            // OpenAI Chat Completions API
            (Method::POST, "/v1/chat/completions") => {
                handle_chat_completions(req, state, &request_id).await
            }

            // OpenAI Responses API (used by Codex CLI)
            (Method::POST, "/v1/responses") => {
                handle_responses(req, state, &request_id).await
            }

            // Token counting API (not implemented - matches JS proxy behavior)
            (Method::POST, "/v1/messages/count_tokens") => Ok(json_response(
                StatusCode::NOT_IMPLEMENTED,
                r#"{"type":"error","error":{"type":"not_implemented","message":"Token counting is not implemented. Use /v1/messages with max_tokens or configure your client to skip token counting."}}"#,
            )),

            // Event logging batch (Claude Code sends these - acknowledge silently)
            (Method::POST, "/api/event_logging/batch") => {
                Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
            }

            // Claude Code heartbeat/event requests to root
            (Method::POST, "/") => {
                Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
            }

            // Models API
            (Method::GET, "/v1/models") => handle_models().await,

            // Stats API
            (Method::GET, "/stats") | (Method::GET, "/v1/stats") => handle_stats(&state).await,

            // Cache stats endpoint
            (Method::GET, "/cache/stats") => {
                let cache = state.cache.lock().await;
                let stats = cache.stats();
                let json = serde_json::to_string(&stats)?;
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(json)))
                    .unwrap())
            }

            // Cache clear endpoint
            (Method::POST, "/cache/clear") => {
                let mut cache = state.cache.lock().await;
                cache.clear();
                Ok(json_response(StatusCode::OK, r#"{"status":"cleared"}"#))
            }

            // Account limits API (quota info for OpenCode)
            (Method::GET, "/account-limits") => handle_account_limits(&state).await,

            // Log streaming API (SSE for OpenCode)
            (Method::GET, "/api/logs/stream") => handle_logs_stream().await,

            // Health check
            (Method::GET, "/health") | (Method::GET, "/") => {
                Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
            }

            // 404 for everything else
            _ => Ok(json_response(
                StatusCode::NOT_FOUND,
                r#"{"type":"error","error":{"type":"not_found","message":"Not found"}}"#,
            )),
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            warn!(request_id = %request_id, "Request timed out");
            Err(Error::Timeout(request_timeout))
        }
    };

    let duration = start.elapsed();

    match &response {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // Don't warn for expected 501 on count_tokens - it's not implemented by design
            let is_expected_501 = status == 501 && path == "/v1/messages/count_tokens";
            if status >= 400 && !is_expected_501 {
                warn!(
                    method = %method,
                    path = %path,
                    status = status,
                    duration_ms = duration.as_millis(),
                    request_id = %request_id,
                    "Request failed"
                );
            } else if status >= 400 {
                debug!(
                    method = %method,
                    path = %path,
                    status = status,
                    request_id = %request_id,
                    "Token counting not implemented"
                );
            } else {
                info!(
                    method = %method,
                    path = %path,
                    status = status,
                    duration_ms = duration.as_millis(),
                    request_id = %request_id,
                    "Request completed"
                );
            }
            Ok(resp.clone())
        }
        Err(e) => {
            let resp = error_to_response(e, &request_id);
            warn!(
                method = %method,
                path = %path,
                status = resp.status().as_u16(),
                duration_ms = duration.as_millis(),
                request_id = %request_id,
                error = %e,
                "Request error"
            );
            Ok(resp)
        }
    }
}

fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("req_{:016x}", nanos)
}

/// Get access token and project ID using account selection strategy
/// Returns (access_token, project_id, account_id, account_email)
async fn get_account_credentials(
    state: &Arc<ServerState>,
    model: &str,
) -> Result<(String, String, String, String), Error> {
    let mut accounts = state.accounts.write().await;

    let account_id = accounts.select_account(model).ok_or_else(|| {
        Error::Auth(AuthError::OAuthFailed(
            "No enabled accounts available. Run 'agcp login' to add an account.".to_string(),
        ))
    })?;

    let account = accounts.get_account_mut(&account_id).ok_or_else(|| {
        Error::Auth(AuthError::OAuthFailed(
            "Selected account not found".to_string(),
        ))
    })?;

    let access_token = account.get_access_token(&state.http_client).await?;
    let project_id = account.project_id.clone().unwrap_or_default();
    let id = account.id.clone();
    let email = account.email.clone();

    // Update last_used timestamp and consume a token
    account.last_used = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    account.consume_token();

    drop(accounts);

    debug!(
        model = %model,
        account_id = %&id[..8.min(id.len())],
        project_id = %project_id,
        "Using account credentials"
    );

    Ok((access_token, project_id, id, email))
}

/// Record request outcome for an account
async fn record_request_outcome(
    state: &Arc<ServerState>,
    account_id: &str,
    model: &str,
    success: bool,
    rate_limit_until: Option<u64>,
) {
    let mut accounts = state.accounts.write().await;

    if let Some(account) = accounts.get_account_mut(account_id) {
        if success {
            account.record_success();
            account.clear_rate_limit(model);
        } else {
            account.record_failure();
            if let Some(until) = rate_limit_until {
                account.set_rate_limit(model, until);
                debug!(
                    account = %&account_id[..8],
                    model = %model,
                    until = until,
                    "Set rate limit for account"
                );
            }
        }
    }

    // Periodically save account state (simple approach: save on failure or rate limit)
    if (!success || rate_limit_until.is_some())
        && let Err(e) = accounts.save()
    {
        warn!(error = %e, "Failed to save account state");
    }
}

/// Extract outcome from a request result, log it, and record it for account health tracking.
async fn track_request_outcome(
    state: &Arc<ServerState>,
    account_id: &str,
    account_email: &str,
    model: &str,
    request_id: &str,
    result: &Result<Response<Full<Bytes>>, Error>,
) {
    let (success, rate_limit_until) = match result {
        Ok(_) => {
            info!(
                model = %model,
                request_id = %request_id,
                account = %account_email,
                "Model used"
            );
            (true, None)
        }
        Err(Error::Api(ApiError::RateLimited { retry_after })) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let until = now + retry_after.as_secs();
            (false, Some(until))
        }
        Err(Error::Api(ApiError::QuotaExhausted { reset_time, .. })) => {
            let until = chrono::DateTime::parse_from_rfc3339(reset_time)
                .ok()
                .map(|dt| dt.timestamp() as u64);
            (false, until)
        }
        Err(_) => (false, None),
    };

    record_request_outcome(state, account_id, model, success, rate_limit_until).await;
}

async fn handle_messages(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    // Extract headers before consuming request
    let bypass_cache = should_bypass_cache(req.headers());

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_type.contains("application/json") {
        return Err(Error::Api(ApiError::InvalidRequest {
            message: "Content-Type must be application/json".to_string(),
        }));
    }

    if let Some(len) = req.headers().get("content-length")
        && let Ok(len_str) = len.to_str()
        && let Ok(len) = len_str.parse::<usize>()
        && len > MAX_REQUEST_SIZE
    {
        return Err(Error::Api(ApiError::RequestTooLarge {
            size: len,
            max: MAX_REQUEST_SIZE,
        }));
    }

    let body_bytes = read_body_limited(req.into_body(), MAX_REQUEST_SIZE).await?;

    let mut messages_request: MessagesRequest = serde_json::from_slice(&body_bytes)?;

    // Resolve model aliases (e.g., "opus" -> "claude-opus-4-6-thinking")
    let original_model = messages_request.model.clone();
    messages_request.model = resolve_model_alias(&messages_request.model).to_string();

    debug!(
        original_model = %original_model,
        resolved_model = %messages_request.model,
        request_id = %request_id,
        "Model resolution"
    );

    validate_request(&messages_request)?;

    // Try the primary model first
    let result =
        execute_messages_request(&messages_request, &state, request_id, false, bypass_cache).await;

    // Check if fallback is enabled and we got a quota exhaustion error
    let config = get_config();
    if config.accounts.fallback
        && let Err(Error::Api(ApiError::QuotaExhausted { .. })) = &result
        && let Some(fallback_model) = get_fallback_model(&messages_request.model)
    {
        warn!(
            primary = %messages_request.model,
            fallback = %fallback_model,
            request_id = %request_id,
            "Quota exhausted, falling back to alternate model"
        );

        let mut fallback_request = messages_request.clone();
        fallback_request.model = fallback_model.to_string();

        return execute_messages_request(&fallback_request, &state, request_id, true, bypass_cache)
            .await;
    }

    result
}

/// Execute a messages request with the given model.
/// Set `is_fallback` to true to prevent recursive fallback attempts.
async fn execute_messages_request(
    messages_request: &MessagesRequest,
    state: &Arc<ServerState>,
    request_id: &str,
    is_fallback: bool,
    bypass_cache: bool,
) -> Result<Response<Full<Bytes>>, Error> {
    let is_streaming = messages_request.stream;
    let model = &messages_request.model;

    get_stats().record_request(model, "/v1/messages");

    debug!(
        model = %model,
        streaming = is_streaming,
        max_tokens = messages_request.max_tokens,
        request_id = %request_id,
        is_fallback = is_fallback,
        "Processing messages request"
    );

    log_if_enabled(request_id, "Anthropic request", &messages_request);

    let cache_key = if !is_streaming && !bypass_cache {
        let messages_json = serde_json::to_string(&messages_request.messages).unwrap_or_default();
        let system_json = messages_request
            .system
            .as_ref()
            .map(|s| serde_json::to_string(s).unwrap_or_default());
        let tools_json = messages_request
            .tools
            .as_ref()
            .map(|t| serde_json::to_string(t).unwrap_or_default());

        let key = ResponseCache::make_key(
            model,
            &messages_json,
            system_json.as_deref(),
            tools_json.as_deref(),
            messages_request.temperature,
        );

        {
            let mut cache = state.cache.lock().await;
            if let Some(cached_response) = cache.get(&key) {
                debug!(
                    model = %model,
                    request_id = %request_id,
                    "Cache HIT"
                );
                return Ok(json_ok_response(cached_response, request_id, Some("HIT")));
            }
        }
        debug!(model = %model, request_id = %request_id, "Cache MISS");
        Some(key)
    } else {
        None
    };

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(state, model).await?;

    let cc_request = build_request(messages_request, &project_id);
    let request_body = serde_json::to_vec(&cc_request)?;

    // Thinking models must use streaming endpoint even for non-streaming requests
    // (the non-streaming generateContent endpoint returns 429 for thinking models)
    let is_thinking = is_thinking_model(model);

    let result = if is_streaming {
        handle_streaming_messages(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else if is_thinking {
        // Use streaming endpoint but return non-streaming response
        handle_thinking_non_streaming_messages(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else {
        handle_non_streaming_messages(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            &cc_request.request_id,
            cache_key.clone(),
            state,
        )
        .await
    };

    track_request_outcome(
        state,
        &account_id,
        &account_email,
        model,
        &cc_request.request_id,
        &result,
    )
    .await;

    result
}

async fn handle_chat_completions(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_type.contains("application/json") {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Content-Type must be application/json",
            "invalid_request_error",
        ));
    }

    let body_bytes = read_body_limited(req.into_body(), MAX_REQUEST_SIZE).await?;

    let chat_request: ChatCompletionRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {}", e),
                "invalid_request_error",
            ));
        }
    };

    // Check for unsupported n > 1
    if chat_request.n.unwrap_or(1) > 1 {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "n > 1 is not supported",
            "invalid_request_error",
        ));
    }

    let mut messages_request = crate::format::openai_to_anthropic(&chat_request);

    let original_model = messages_request.model.clone();
    messages_request.model = resolve_model_alias(&messages_request.model).to_string();

    debug!(
        original_model = %original_model,
        resolved_model = %messages_request.model,
        request_id = %request_id,
        "Model resolution (OpenAI)"
    );

    validate_request(&messages_request)?;

    // Try the primary model first
    let result = execute_openai_request(&messages_request, &state, request_id, false).await;

    // Check if fallback is enabled and we got a quota exhaustion error
    let config = get_config();
    if config.accounts.fallback
        && let Err(Error::Api(ApiError::QuotaExhausted { .. })) = &result
        && let Some(fallback_model) = get_fallback_model(&messages_request.model)
    {
        warn!(
            primary = %messages_request.model,
            fallback = %fallback_model,
            request_id = %request_id,
            "Quota exhausted, falling back to alternate model (OpenAI API)"
        );

        let mut fallback_request = messages_request.clone();
        fallback_request.model = fallback_model.to_string();

        return execute_openai_request(&fallback_request, &state, request_id, true).await;
    }

    result
}

/// Execute an OpenAI-format request with the given model.
/// Set `is_fallback` to true to prevent recursive fallback attempts.
async fn execute_openai_request(
    messages_request: &MessagesRequest,
    state: &Arc<ServerState>,
    request_id: &str,
    is_fallback: bool,
) -> Result<Response<Full<Bytes>>, Error> {
    let is_streaming = messages_request.stream;
    let model = &messages_request.model;

    get_stats().record_request(model, "/v1/chat/completions");

    debug!(
        model = %model,
        streaming = is_streaming,
        max_tokens = messages_request.max_tokens,
        request_id = %request_id,
        is_fallback = is_fallback,
        "Processing OpenAI chat completions request"
    );

    log_if_enabled(request_id, "OpenAI request", &messages_request);

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(state, model).await?;

    let cc_request = build_request(messages_request, &project_id);
    let request_body = serde_json::to_vec(&cc_request)?;

    let is_thinking = is_thinking_model(model);

    let result = if is_streaming {
        handle_openai_streaming(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else if is_thinking {
        handle_openai_thinking_non_streaming(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else {
        handle_openai_non_streaming(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    };

    track_request_outcome(
        state,
        &account_id,
        &account_email,
        model,
        &cc_request.request_id,
        &result,
    )
    .await;

    result
}

async fn handle_openai_non_streaming(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let response = client.send_request(body, access_token, model).await?;
    let anthropic_response = parse_response(&response, model, request_id);

    let openai_response =
        crate::format::anthropic_to_openai(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "OpenAI response", &openai_response);

    let body = serde_json::to_vec(&openai_response)?;
    Ok(json_ok_response(body, request_id, None))
}

async fn handle_openai_thinking_non_streaming(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let (events, _body_str) = collect_sse_events(client, body, access_token, model).await?;

    check_stream_errors(
        &events,
        model,
        request_id,
        " (OpenAI thinking non-streaming)",
    )?;

    let anthropic_response = crate::format::build_response_from_events(&events, model, request_id);
    let openai_response =
        crate::format::anthropic_to_openai(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "OpenAI response", &openai_response);

    let response_body = serde_json::to_vec(&openai_response)?;
    Ok(json_ok_response(response_body, request_id, Some("BYPASS")))
}

async fn handle_openai_streaming(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    use crate::format::openai::{ChatCompletionChunk, ChatUsage, ChunkChoice, ChunkDelta};
    use std::time::{SystemTime, UNIX_EPOCH};

    let (all_events, _body_str) = collect_sse_events(client, body, access_token, model).await?;
    let mut output = String::new();

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let chunk_id = format!("chatcmpl-{}", request_id);

    check_stream_errors(&all_events, model, request_id, " (OpenAI streaming)")?;

    let mut input_tokens = 0u32;
    let mut sent_role = false;

    for event in &all_events {
        match event {
            StreamEvent::MessageStart { message } => {
                input_tokens = message.usage.input_tokens;
                // Send initial chunk with role
                let chunk = ChatCompletionChunk {
                    id: chunk_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model.to_string(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: Some("assistant".to_string()),
                            content: None,
                            tool_calls: None,
                        },
                        finish_reason: None,
                        logprobs: None,
                    }],
                    usage: None,
                    system_fingerprint: None,
                };
                output.push_str(&format!(
                    "data: {}\n\n",
                    serde_json::to_string(&chunk).unwrap_or_default()
                ));
                sent_role = true;
            }
            StreamEvent::ContentBlockDelta { delta, .. } => {
                match delta {
                    crate::format::ContentDelta::Text { text } => {
                        let chunk = ChatCompletionChunk {
                            id: chunk_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created,
                            model: model.to_string(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChunkDelta {
                                    role: if !sent_role {
                                        Some("assistant".to_string())
                                    } else {
                                        None
                                    },
                                    content: Some(text.clone()),
                                    tool_calls: None,
                                },
                                finish_reason: None,
                                logprobs: None,
                            }],
                            usage: None,
                            system_fingerprint: None,
                        };
                        output.push_str(&format!(
                            "data: {}\n\n",
                            serde_json::to_string(&chunk).unwrap_or_default()
                        ));
                        sent_role = true;
                    }
                    crate::format::ContentDelta::Thinking { thinking } => {
                        // Include thinking as text with markers
                        let chunk = ChatCompletionChunk {
                            id: chunk_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created,
                            model: model.to_string(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChunkDelta {
                                    role: None,
                                    content: Some(thinking.clone()),
                                    tool_calls: None,
                                },
                                finish_reason: None,
                                logprobs: None,
                            }],
                            usage: None,
                            system_fingerprint: None,
                        };
                        output.push_str(&format!(
                            "data: {}\n\n",
                            serde_json::to_string(&chunk).unwrap_or_default()
                        ));
                    }
                    _ => {}
                }
            }
            StreamEvent::MessageDelta { delta, usage } => {
                let output_tokens = usage.output_tokens;
                let finish_reason = delta.stop_reason.map(|r| r.to_openai_str().to_string());

                let chunk = ChatCompletionChunk {
                    id: chunk_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model.to_string(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: None,
                        },
                        finish_reason,
                        logprobs: None,
                    }],
                    usage: Some(ChatUsage {
                        prompt_tokens: input_tokens,
                        completion_tokens: output_tokens,
                        total_tokens: input_tokens + output_tokens,
                    }),
                    system_fingerprint: None,
                };
                output.push_str(&format!(
                    "data: {}\n\n",
                    serde_json::to_string(&chunk).unwrap_or_default()
                ));
            }
            _ => {}
        }
    }

    output.push_str("data: [DONE]\n\n");

    Ok(sse_ok_response(output, request_id))
}

// ============================================================================
// OpenAI Responses API handlers (used by Codex CLI)
// ============================================================================

async fn handle_responses(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_type.contains("application/json") {
        return Ok(responses_error_response(
            StatusCode::BAD_REQUEST,
            "Content-Type must be application/json",
            "invalid_request_error",
        ));
    }

    let body_bytes = read_body_limited(req.into_body(), MAX_REQUEST_SIZE).await?;

    let responses_request: crate::format::ResponsesRequest =
        match serde_json::from_slice(&body_bytes) {
            Ok(r) => r,
            Err(e) => {
                return Ok(responses_error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid JSON: {}", e),
                    "invalid_request_error",
                ));
            }
        };

    // Log the tools for debugging
    if let Some(tools) = &responses_request.tools {
        for tool in tools {
            trace!(
                tool_type = %tool.tool_type,
                tool_name = ?tool.name,
                "Responses API: tool in request"
            );
        }
    }

    let mut messages_request = crate::format::responses_to_anthropic(&responses_request);

    let original_model = messages_request.model.clone();
    messages_request.model = resolve_model_alias(&messages_request.model).to_string();

    debug!(
        original_model = %original_model,
        resolved_model = %messages_request.model,
        request_id = %request_id,
        "Model resolution (Responses)"
    );

    if let Err(e) = validate_request(&messages_request) {
        return Ok(responses_error_response(
            StatusCode::BAD_REQUEST,
            &e.to_string(),
            "invalid_request_error",
        ));
    }

    get_stats().record_request(&messages_request.model, "/v1/responses");

    let is_streaming = messages_request.stream;
    let model = &messages_request.model;

    debug!(
        model = %model,
        streaming = is_streaming,
        request_id = %request_id,
        "Processing Responses API request"
    );

    // Log streaming status explicitly
    debug!(
        request_id = %request_id,
        streaming = is_streaming,
        model = %model,
        "Responses API: handling request"
    );

    log_if_enabled(request_id, "Responses API request", &messages_request);

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(&state, model).await?;

    let cc_request = build_request(&messages_request, &project_id);
    let request_body = serde_json::to_vec(&cc_request)?;

    // Thinking models must use streaming endpoint even for non-streaming requests
    let is_thinking = is_thinking_model(model);

    let result = if is_streaming {
        handle_responses_streaming(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            request_id,
        )
        .await
    } else if is_thinking {
        // Use streaming endpoint but return non-streaming response
        handle_responses_thinking_non_streaming(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            request_id,
        )
        .await
    } else {
        handle_responses_non_streaming(
            &state.cloudcode_client,
            &request_body,
            &access_token,
            model,
            request_id,
        )
        .await
    };

    track_request_outcome(
        &state,
        &account_id,
        &account_email,
        model,
        request_id,
        &result,
    )
    .await;

    result
}

async fn handle_responses_non_streaming(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let response = client.send_request(body, access_token, model).await?;
    let anthropic_response = parse_response(&response, model, request_id);

    let responses_response =
        crate::format::anthropic_to_responses(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "Responses API response", &responses_response);

    let body = serde_json::to_vec(&responses_response)?;
    Ok(json_ok_response(body, request_id, None))
}

// Thinking models must use streaming endpoint but return non-streaming response
async fn handle_responses_thinking_non_streaming(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let (all_events, _body_str) = collect_sse_events(client, body, access_token, model).await?;

    check_stream_errors(
        &all_events,
        model,
        request_id,
        " (Responses thinking non-streaming)",
    )?;

    let anthropic_response =
        crate::format::build_response_from_events(&all_events, model, request_id);

    let responses_response =
        crate::format::anthropic_to_responses(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "Responses API response", &responses_response);

    let body = serde_json::to_vec(&responses_response)?;
    Ok(json_ok_response(body, request_id, None))
}

async fn handle_responses_streaming(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    use crate::format::responses::{
        InputTokensDetails, OutputTokensDetails, ResponseOutputContent, ResponseOutputItem,
        ResponseStreamEvent, ResponseUsage, ResponsesResponse,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    let (all_events, _body_str) = collect_sse_events(client, body, access_token, model).await?;
    let mut output = String::new();

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let resp_id = format!("resp_{}", request_id);

    check_stream_errors(&all_events, model, request_id, " (Responses streaming)")?;

    // Log event count for debugging
    trace!(
        request_id = %request_id,
        event_count = all_events.len(),
        "Responses streaming: parsed events"
    );

    // Log each event type for debugging
    for (i, event) in all_events.iter().enumerate() {
        let event_type = match event {
            StreamEvent::MessageStart { .. } => "MessageStart",
            StreamEvent::ContentBlockStart { .. } => "ContentBlockStart",
            StreamEvent::ContentBlockDelta { .. } => "ContentBlockDelta",
            StreamEvent::ContentBlockStop { .. } => "ContentBlockStop",
            StreamEvent::MessageDelta { .. } => "MessageDelta",
            StreamEvent::MessageStop => "MessageStop",
            StreamEvent::Ping => "Ping",
            StreamEvent::Error { .. } => "Error",
        };
        trace!(request_id = %request_id, index = i, event_type = event_type, "Event");
    }

    // Track state for building response
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    let mut cache_read_tokens = 0u32;
    let mut reasoning_tokens = 0u32;
    let mut text_content = String::new();
    let mut reasoning_content = String::new();
    let mut sent_initial = false;
    let mut message_added = false;
    let mut output_index = 0usize;
    let content_index = 0usize;

    // Track tool calls
    let mut tool_calls: Vec<(String, String, String)> = vec![]; // (id, name, arguments)
    let mut current_tool_json = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();

    // Helper to emit SSE event
    fn emit_event(output: &mut String, event: &ResponseStreamEvent) {
        output.push_str("data: ");
        output.push_str(&serde_json::to_string(event).unwrap_or_default());
        output.push_str("\n\n");
    }

    // Create initial empty response for created event
    let make_response = |status: &'static str,
                         out: Vec<ResponseOutputItem>,
                         usage: Option<ResponseUsage>|
     -> ResponsesResponse {
        ResponsesResponse {
            id: resp_id.clone(),
            object: "response",
            created_at,
            model: model.to_string(),
            output: out,
            parallel_tool_calls: true,
            tool_choice: "auto",
            tools: vec![],
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            usage,
            status,
        }
    };

    for event in &all_events {
        match event {
            StreamEvent::MessageStart { message } => {
                input_tokens = message.usage.input_tokens;
                cache_read_tokens = message.usage.cache_read_input_tokens.unwrap_or(0);

                if !sent_initial {
                    // Emit response.created
                    emit_event(
                        &mut output,
                        &ResponseStreamEvent::ResponseCreated {
                            response: make_response("in_progress", vec![], None),
                        },
                    );
                    sent_initial = true;
                }
            }
            StreamEvent::ContentBlockStart {
                content_block,
                index,
            } => {
                let block_type = match content_block {
                    crate::format::ContentBlock::Text { .. } => "Text",
                    crate::format::ContentBlock::Thinking { .. } => "Thinking",
                    crate::format::ContentBlock::ToolUse { .. } => "ToolUse",
                    _ => "Other",
                };
                trace!(request_id = %request_id, block_type = block_type, index = index, "ContentBlockStart type");

                match content_block {
                    crate::format::ContentBlock::Text { .. } => {
                        if !message_added {
                            // Add the message output item
                            let msg_item = ResponseOutputItem::Message {
                                id: format!("msg_{}", &request_id[..8.min(request_id.len())]),
                                role: "assistant",
                                status: "in_progress",
                                content: vec![],
                            };
                            emit_event(
                                &mut output,
                                &ResponseStreamEvent::OutputItemAdded {
                                    output_index,
                                    item: msg_item,
                                },
                            );

                            // Add content part
                            let part = ResponseOutputContent::OutputText {
                                text: String::new(),
                                annotations: vec![],
                            };
                            emit_event(
                                &mut output,
                                &ResponseStreamEvent::ContentPartAdded {
                                    output_index,
                                    content_index,
                                    part,
                                },
                            );
                            message_added = true;
                        }
                    }
                    crate::format::ContentBlock::ToolUse { id, name, .. } => {
                        // Start tracking this tool call
                        current_tool_id = id.clone();
                        current_tool_name = name.clone();
                        current_tool_json.clear();

                        // Emit function call added event
                        let fc_item = ResponseOutputItem::FunctionCall {
                            id: format!("fc_{}", id),
                            call_id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                            status: "in_progress",
                        };
                        emit_event(
                            &mut output,
                            &ResponseStreamEvent::OutputItemAdded {
                                output_index,
                                item: fc_item,
                            },
                        );
                        output_index += 1;
                    }
                    _ => {}
                }
            }
            StreamEvent::ContentBlockDelta { delta, .. } => {
                match delta {
                    crate::format::ContentDelta::Text { text } => {
                        text_content.push_str(text);
                        emit_event(
                            &mut output,
                            &ResponseStreamEvent::OutputTextDelta {
                                output_index,
                                content_index,
                                delta: text.clone(),
                            },
                        );
                    }
                    crate::format::ContentDelta::Thinking { thinking } => {
                        reasoning_content.push_str(thinking);
                        reasoning_tokens += 1; // Approximate
                    }
                    crate::format::ContentDelta::InputJson { partial_json } => {
                        // Accumulate tool call arguments
                        current_tool_json.push_str(partial_json);
                    }
                    _ => {}
                }
            }
            StreamEvent::ContentBlockStop { .. } => {
                // If we were building a tool call, finalize it
                if !current_tool_id.is_empty() {
                    debug!(
                        request_id = %request_id,
                        tool_id = %current_tool_id,
                        tool_name = %current_tool_name,
                        "Tool call finalized"
                    );
                    tool_calls.push((
                        current_tool_id.clone(),
                        current_tool_name.clone(),
                        current_tool_json.clone(),
                    ));

                    // Emit function call done
                    let fc_item = ResponseOutputItem::FunctionCall {
                        id: format!("fc_{}", &current_tool_id),
                        call_id: current_tool_id.clone(),
                        name: current_tool_name.clone(),
                        arguments: current_tool_json.clone(),
                        status: "completed",
                    };
                    emit_event(
                        &mut output,
                        &ResponseStreamEvent::OutputItemDone {
                            output_index: output_index.saturating_sub(1),
                            item: fc_item,
                        },
                    );

                    current_tool_id.clear();
                    current_tool_name.clear();
                    current_tool_json.clear();
                }
            }
            StreamEvent::MessageDelta { usage, .. } => {
                output_tokens = usage.output_tokens;
            }
            StreamEvent::MessageStop => {
                // Message complete
            }
            _ => {}
        }
    }

    // Emit final events
    if message_added {
        // Text done
        emit_event(
            &mut output,
            &ResponseStreamEvent::OutputTextDone {
                output_index,
                content_index,
                text: text_content.clone(),
            },
        );

        // Content part done
        let part = ResponseOutputContent::OutputText {
            text: text_content.clone(),
            annotations: vec![],
        };
        emit_event(
            &mut output,
            &ResponseStreamEvent::ContentPartDone {
                output_index,
                content_index,
                part: part.clone(),
            },
        );

        // Output item done
        let msg_item = ResponseOutputItem::Message {
            id: format!("msg_{}", &request_id[..8.min(request_id.len())]),
            role: "assistant",
            status: "completed",
            content: vec![part],
        };
        emit_event(
            &mut output,
            &ResponseStreamEvent::OutputItemDone {
                output_index,
                item: msg_item.clone(),
            },
        );
    }

    // Build final usage
    let usage = Some(ResponseUsage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        input_tokens_details: if cache_read_tokens > 0 {
            Some(InputTokensDetails {
                cached_tokens: cache_read_tokens,
            })
        } else {
            None
        },
        output_tokens_details: if reasoning_tokens > 0 {
            Some(OutputTokensDetails { reasoning_tokens })
        } else {
            None
        },
    });

    // Log final output size for debugging
    debug!(
        request_id = %request_id,
        output_len = output.len(),
        text_len = text_content.len(),
        tool_calls = tool_calls.len(),
        "Responses streaming: sending response"
    );

    // Build final output
    let mut final_output = vec![];

    // Add tool calls first
    for (id, name, arguments) in &tool_calls {
        final_output.push(ResponseOutputItem::FunctionCall {
            id: format!("fc_{}", id),
            call_id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
            status: "completed",
        });
    }

    if !reasoning_content.is_empty() {
        final_output.push(ResponseOutputItem::Reasoning {
            id: format!("rs_{}", &request_id[..8.min(request_id.len())]),
            status: "completed",
            summary: Some(vec![ResponseOutputContent::OutputText {
                text: reasoning_content,
                annotations: vec![],
            }]),
        });
    }
    if !text_content.is_empty() {
        final_output.push(ResponseOutputItem::Message {
            id: format!("msg_{}", &request_id[..8.min(request_id.len())]),
            role: "assistant",
            status: "completed",
            content: vec![ResponseOutputContent::OutputText {
                text: text_content,
                annotations: vec![],
            }],
        });
    }

    // Response completed
    emit_event(
        &mut output,
        &ResponseStreamEvent::ResponseCompleted {
            response: make_response("completed", final_output, usage),
        },
    );

    output.push_str("data: [DONE]\n\n");

    Ok(sse_ok_response(output, request_id))
}

/// Error response format
#[derive(Clone, Copy)]
enum ErrorFormat {
    /// OpenAI format: error.{message, type, param: null, code: null}
    OpenAI,
    /// Responses format: error.{message, type, code: type}
    Responses,
}

fn error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
    format: ErrorFormat,
) -> Response<Full<Bytes>> {
    let body = match format {
        ErrorFormat::OpenAI => serde_json::json!({
            "error": {
                "message": message,
                "type": error_type,
                "param": null,
                "code": null
            }
        }),
        ErrorFormat::Responses => serde_json::json!({
            "error": {
                "message": message,
                "type": error_type,
                "code": error_type
            }
        }),
    }
    .to_string();

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("Response construction with valid headers should not fail")
}

fn responses_error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
) -> Response<Full<Bytes>> {
    error_response(status, message, error_type, ErrorFormat::Responses)
}

fn openai_error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
) -> Response<Full<Bytes>> {
    error_response(status, message, error_type, ErrorFormat::OpenAI)
}

/// Check if request headers indicate cache bypass
fn should_bypass_cache(headers: &hyper::HeaderMap) -> bool {
    // Check Cache-Control: no-cache or no-store
    if let Some(cc) = headers.get("cache-control")
        && let Ok(s) = cc.to_str()
        && (s.contains("no-cache") || s.contains("no-store"))
    {
        return true;
    }

    // Check X-No-Cache header
    if let Some(nc) = headers.get("x-no-cache")
        && let Ok(s) = nc.to_str()
        && (s == "true" || s == "1")
    {
        return true;
    }

    false
}

fn validate_request(req: &MessagesRequest) -> Result<(), Error> {
    if req.max_tokens == 0 {
        return Err(Error::Api(ApiError::InvalidRequest {
            message: "max_tokens must be greater than 0".to_string(),
        }));
    }

    if req.max_tokens > 200_000 {
        return Err(Error::Api(ApiError::InvalidRequest {
            message: "max_tokens cannot exceed 200000".to_string(),
        }));
    }

    if req.model.is_empty() {
        return Err(Error::Api(ApiError::InvalidRequest {
            message: "model is required".to_string(),
        }));
    }

    if req.messages.is_empty() {
        return Err(Error::Api(ApiError::InvalidRequest {
            message: "messages array cannot be empty".to_string(),
        }));
    }

    if let Some(temp) = req.temperature
        && !(0.0..=2.0).contains(&temp)
    {
        return Err(Error::Api(ApiError::InvalidRequest {
            message: "temperature must be between 0.0 and 2.0".to_string(),
        }));
    }

    Ok(())
}

async fn read_body_limited(body: hyper::body::Incoming, max_size: usize) -> Result<Vec<u8>, Error> {
    let collected = body
        .collect()
        .await
        .map_err(|e| Error::Http(e.to_string()))?;

    let bytes = collected.to_bytes();
    if bytes.len() > max_size {
        return Err(Error::Api(ApiError::RequestTooLarge {
            size: bytes.len(),
            max: max_size,
        }));
    }

    Ok(bytes.to_vec())
}

async fn handle_non_streaming_messages(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
    cache_key: Option<String>,
    state: &Arc<ServerState>,
) -> Result<Response<Full<Bytes>>, Error> {
    let response = client.send_request(body, access_token, model).await?;
    let anthropic_response = parse_response(&response, model, request_id);

    log_if_enabled(request_id, "Anthropic response", &anthropic_response);

    let response_bytes = serde_json::to_vec(&anthropic_response)?;

    if let Some(ref key) = cache_key {
        let mut cache = state.cache.lock().await;
        cache.put(key.clone(), response_bytes.clone());
        debug!(model = %model, request_id = %request_id, "Cached response");
    }

    // Add X-Cache header: MISS if we have a cache key (means we tried cache but didn't hit)
    let cache_header = if cache_key.is_some() {
        "MISS"
    } else {
        "BYPASS"
    };

    Ok(json_ok_response(
        response_bytes,
        request_id,
        Some(cache_header),
    ))
}

// Thinking models must use streaming endpoint (doesn't rate-limit) but client may want non-streaming
async fn handle_thinking_non_streaming_messages(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let (events, body_str) = collect_sse_events(client, body, access_token, model).await?;

    // Log raw response for debugging empty/error responses
    if body_str.len() < 2000 {
        debug!(
            model = %model,
            request_id = %request_id,
            body_len = body_str.len(),
            body = %body_str,
            "Raw SSE response from Google (thinking non-streaming)"
        );
    } else {
        debug!(
            model = %model,
            request_id = %request_id,
            body_len = body_str.len(),
            "Raw SSE response from Google (thinking non-streaming, truncated)"
        );
    }

    check_stream_errors(&events, model, request_id, " (thinking non-streaming)")?;

    // Check if we got an empty response (no content events)
    let has_content = events.iter().any(|e| {
        matches!(
            e,
            StreamEvent::ContentBlockStart { .. } | StreamEvent::ContentBlockDelta { .. }
        )
    });

    if !has_content && !body_str.is_empty() {
        warn!(
            model = %model,
            request_id = %request_id,
            body_len = body_str.len(),
            "Empty response from Google API (thinking non-streaming) - model may be unavailable. Raw body: {}",
            body_str.chars().take(500).collect::<String>()
        );

        // Return an error instead of an empty response
        return Err(Error::Api(ApiError::ServerError {
            status: 502,
            message: format!(
                "Model {} returned empty response from Google API. The model may be unavailable. Raw: {}",
                model,
                body_str.chars().take(200).collect::<String>()
            ),
        }));
    }

    let anthropic_response = crate::format::build_response_from_events(&events, model, request_id);

    log_if_enabled(request_id, "Anthropic response", &anthropic_response);

    let response_body = serde_json::to_vec(&anthropic_response)?;
    Ok(json_ok_response(response_body, request_id, Some("BYPASS")))
}

async fn handle_streaming_messages(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, Error> {
    let (events, body_str) = collect_sse_events(client, body, access_token, model).await?;
    let mut output = String::new();

    // Log raw response for debugging empty/error responses
    if body_str.len() < 2000 {
        debug!(
            model = %model,
            request_id = %request_id,
            body_len = body_str.len(),
            body = %body_str,
            "Raw SSE response from Google"
        );
    } else {
        debug!(
            model = %model,
            request_id = %request_id,
            body_len = body_str.len(),
            "Raw SSE response from Google (truncated, too large to log)"
        );
    }

    check_stream_errors(&events, model, request_id, "")?;

    for event in &events {
        output.push_str(&format_sse_event(event));
    }
    output.push_str(&format_sse_event(&create_message_stop()));

    // Check if we got an empty response (no content events)
    let has_content = events.iter().any(|e| {
        matches!(
            e,
            StreamEvent::ContentBlockStart { .. } | StreamEvent::ContentBlockDelta { .. }
        )
    });

    if !has_content && !body_str.is_empty() {
        warn!(
            model = %model,
            request_id = %request_id,
            body_len = body_str.len(),
            "Empty response from Google API - model may be unavailable. Raw body: {}",
            body_str.chars().take(500).collect::<String>()
        );

        // Return an error instead of an empty SSE stream
        return Err(Error::Api(ApiError::ServerError {
            status: 502,
            message: format!(
                "Model {} returned empty response from Google API. The model may be unavailable. Raw: {}",
                model,
                body_str.chars().take(200).collect::<String>()
            ),
        }));
    }

    Ok(sse_ok_response(output, request_id))
}

async fn handle_models() -> Result<Response<Full<Bytes>>, Error> {
    let models: Vec<ModelInfo> = Model::all()
        .iter()
        .map(|m| ModelInfo {
            id: m.anthropic_id().to_string(),
            model_type: "model".to_string(),
            display_name: m.anthropic_id().to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .collect();

    let response = ModelsResponse { data: models };
    let body = serde_json::to_vec(&response)?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

async fn handle_stats(state: &Arc<ServerState>) -> Result<Response<Full<Bytes>>, Error> {
    let stats = get_stats().summary();
    let cache_stats = state.cache.lock().await.stats();

    let response = serde_json::json!({
        "requests": stats.to_json(),
        "cache": cache_stats,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(response.to_string())))
        .unwrap())
}

async fn handle_account_limits(state: &Arc<ServerState>) -> Result<Response<Full<Bytes>>, Error> {
    // Get credentials using the existing pattern
    let credentials = get_account_credentials(state, "claude-sonnet-4-5").await;

    let response = match credentials {
        Ok((access_token, project_id, account_id, _account_email)) => {
            match fetch_model_quotas(&state.http_client, &access_token, Some(&project_id)).await {
                Ok(quotas) => {
                    // Save quota data to the account for TUI display
                    {
                        let mut accounts = state.accounts.write().await;
                        if let Some(account) = accounts.get_account_mut(&account_id) {
                            for q in &quotas {
                                // Parse ISO timestamp to Unix timestamp
                                let reset_time = q
                                    .reset_time
                                    .as_ref()
                                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                    .map(|dt| dt.timestamp() as u64)
                                    .unwrap_or(0);

                                account.quota.insert(
                                    q.model_id.clone(),
                                    crate::auth::accounts::ModelQuota {
                                        remaining_fraction: q.remaining_fraction,
                                        reset_time,
                                    },
                                );
                            }
                            // Save to disk
                            if let Err(e) = accounts.save() {
                                warn!(error = %e, "Failed to save quota data");
                            }
                        }
                    }

                    let models: Vec<serde_json::Value> = quotas
                        .iter()
                        .map(|q| {
                            serde_json::json!({
                                "model": q.model_id,
                                "remaining_fraction": q.remaining_fraction,
                                "reset_time": q.reset_time,
                            })
                        })
                        .collect();

                    serde_json::json!({
                        "status": "ok",
                        "quotas": models,
                    })
                }
                Err(e) => {
                    serde_json::json!({
                        "status": "error",
                        "message": e,
                    })
                }
            }
        }
        Err(e) => {
            serde_json::json!({
                "status": "error",
                "message": e.to_string(),
            })
        }
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(response.to_string())))
        .unwrap())
}

async fn handle_logs_stream() -> Result<Response<Full<Bytes>>, Error> {
    // Get the log file path
    let config_dir = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config").join("agcp"))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log_path = config_dir.join("agcp.log");

    // Read the last 100 lines from the log file
    let lines = match std::fs::File::open(&log_path) {
        Ok(file) => {
            let reader = std::io::BufReader::new(file);
            let all_lines: Vec<String> = std::io::BufRead::lines(reader)
                .map_while(Result::ok)
                .collect();
            let start = all_lines.len().saturating_sub(100);
            all_lines[start..].to_vec()
        }
        Err(_) => vec!["No log file available".to_string()],
    };

    // Format as SSE events
    let mut body = String::new();
    for line in lines {
        // Strip ANSI codes for cleaner output
        let clean_line = strip_ansi_codes(&line);
        body.push_str(&format!("data: {}\n\n", clean_line));
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip escape sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Skip until we hit a letter (end of sequence)
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Helper to create a test server state with default (empty) accounts and in-memory cache.
#[cfg(test)]
fn test_server_state() -> Arc<ServerState> {
    use crate::auth::accounts::AccountStore;

    Arc::new(ServerState {
        accounts: RwLock::new(AccountStore::default()),
        http_client: HttpClient::default(),
        cloudcode_client: CloudCodeClient::default(),
        cache: Mutex::new(ResponseCache::new(true, 300, 100)),
    })
}

/// Send a streaming request and collect all SSE events by parsing the full response body.
async fn collect_sse_events(
    client: &CloudCodeClient,
    body: &[u8],
    access_token: &str,
    model: &str,
) -> Result<(Vec<StreamEvent>, String), Error> {
    let response = client
        .send_streaming_request(body, access_token, model)
        .await?;

    let mut parser = SseParser::new(model);

    let body_bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| Error::Http(e.to_string()))?
        .to_bytes();

    let body_str = String::from_utf8_lossy(&body_bytes).into_owned();

    let mut events = Vec::new();
    for event in parser.feed(&body_str) {
        events.push(event);
    }
    for event in parser.finish() {
        events.push(event);
    }

    Ok((events, body_str))
}

/// Check SSE events for API errors and return an error if one is found.
fn check_stream_errors(
    events: &[StreamEvent],
    model: &str,
    request_id: &str,
    context: &str,
) -> Result<(), Error> {
    let api_error = events.iter().find_map(|e| {
        if let StreamEvent::Error { error } = e {
            Some(error.message.clone())
        } else {
            None
        }
    });

    if let Some(error_message) = api_error {
        warn!(
            model = %model,
            request_id = %request_id,
            error = %error_message,
            "Google API returned error in SSE stream{context}"
        );
        return Err(Error::Api(ApiError::ServerError {
            status: 502,
            message: error_message,
        }));
    }

    Ok(())
}

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

/// Log a serializable value as pretty-printed JSON if request logging is enabled.
fn log_if_enabled<T: serde::Serialize>(request_id: &str, label: &str, value: &T) {
    if get_config().logging.log_requests
        && let Ok(json) = serde_json::to_string_pretty(value)
    {
        info!(request_id = %request_id, "{}:\n{}", label, json);
    }
}

/// Build a JSON OK response with request tracking headers.
fn json_ok_response(body: Vec<u8>, request_id: &str, cache: Option<&str>) -> Response<Full<Bytes>> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("X-Request-Id", request_id);

    if let Some(cache_status) = cache {
        builder = builder.header("X-Cache", cache_status);
    }

    builder.body(Full::new(Bytes::from(body))).unwrap()
}

/// Build an SSE streaming response with standard headers.
fn sse_ok_response(body: String, request_id: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .header("X-Request-Id", request_id)
        .header("X-Cache", "BYPASS")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn error_to_response(error: &Error, request_id: &str) -> Response<Full<Bytes>> {
    let (status, error_type, message) = match error {
        Error::Auth(AuthError::TokenExpired) => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Token expired".to_string(),
        ),
        Error::Auth(e) => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            e.to_string(),
        ),
        Error::Api(ApiError::RateLimited { retry_after }) => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            format!("Rate limited. Retry after {:?}", retry_after),
        ),
        Error::Api(ApiError::QuotaExhausted { model, reset_time }) => (
            StatusCode::TOO_MANY_REQUESTS,
            "invalid_request_error",
            format!(
                "You have exhausted your capacity on {model}. Quota will reset after {reset_time}."
            ),
        ),
        Error::Api(ApiError::InvalidRequest { message }) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message.clone(),
        ),
        Error::Api(ApiError::ServerError { status, message }) => (
            StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            "api_error",
            message.clone(),
        ),
        Error::Api(ApiError::CapacityExhausted) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "Model capacity exhausted".to_string(),
        ),
        Error::Api(ApiError::RequestTooLarge { size, max }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            format!(
                "Request body too large: {} bytes (max: {} bytes)",
                size, max
            ),
        ),
        Error::Io(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            e.to_string(),
        ),
        Error::Json(e) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            e.to_string(),
        ),
        Error::Http(msg) => (StatusCode::BAD_GATEWAY, "api_error", msg.clone()),
        Error::Timeout(d) => (
            StatusCode::GATEWAY_TIMEOUT,
            "timeout_error",
            format!("Request timed out after {:?}", d),
        ),
    };

    // Add suggestion if available
    let message_with_suggestion = if let Some(suggestion) = error.suggestion() {
        format!("{}. {}", message, suggestion)
    } else {
        message
    };

    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message_with_suggestion
        },
        "request_id": request_id
    })
    .to_string();

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("X-Request-Id", request_id)
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Spin up the server on a random port and return the bound address.
    async fn spawn_test_server() -> SocketAddr {
        let state = test_server_state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                if let Ok((stream, remote_addr)) = listener.accept().await {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, remote_addr, state).await;
                    });
                }
            }
        });

        addr
    }

    /// Send a raw HTTP/1.1 request and return (status_code, body).
    async fn http_request(addr: SocketAddr, request: &str) -> (u16, String) {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        // Read the full response (server closes connection due to Connection: close)
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;

        let response = String::from_utf8_lossy(&buf).to_string();

        // Parse status code from first line: "HTTP/1.1 200 OK"
        let status_code = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);

        // Extract body: everything after the blank line (\r\n\r\n)
        // For chunked encoding, just grab everything for assertion matching
        let body = response
            .split("\r\n\r\n")
            .skip(1)
            .collect::<Vec<_>>()
            .join("");

        (status_code, body)
    }

    // -- Health check --

    #[tokio::test]
    async fn test_health_check() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""status":"ok"#), "body: {body}");
    }

    #[tokio::test]
    async fn test_root_get_health() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""status":"ok"#), "body: {body}");
    }

    // -- Models --

    #[tokio::test]
    async fn test_models_endpoint() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /v1/models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(
            body.contains("claude-opus-4-6-thinking"),
            "body should list Claude models: {body}"
        );
        assert!(
            body.contains("gemini-2.5-flash"),
            "body should list Gemini models: {body}"
        );
    }

    // -- 404 --

    #[tokio::test]
    async fn test_not_found() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /nonexistent HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 404, "body: {body}");
        assert!(body.contains("not_found"), "body: {body}");
    }

    // -- Token counting (not implemented) --

    #[tokio::test]
    async fn test_count_tokens_not_implemented() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "POST /v1/messages/count_tokens HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert_eq!(status, 501, "body: {body}");
        assert!(body.contains("not_implemented"), "body: {body}");
    }

    // -- Event logging batch --

    #[tokio::test]
    async fn test_event_logging_batch() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "POST /api/event_logging/batch HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""status":"ok"#), "body: {body}");
    }

    // -- POST / heartbeat --

    #[tokio::test]
    async fn test_post_root_heartbeat() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "POST / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""status":"ok"#), "body: {body}");
    }

    // -- Cache endpoints --

    #[tokio::test]
    async fn test_cache_stats() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /cache/stats HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
    }

    #[tokio::test]
    async fn test_cache_clear() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "POST /cache/clear HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("cleared"), "body: {body}");
    }

    // -- Stats endpoint --

    #[tokio::test]
    async fn test_stats_endpoint() {
        let addr = spawn_test_server().await;
        let (status, _body) = http_request(
            addr,
            "GET /stats HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200);
    }

    // -- Messages endpoint: validation errors --

    #[tokio::test]
    async fn test_messages_invalid_json() {
        let addr = spawn_test_server().await;
        let (status, _body) = http_request(
            addr,
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 14\r\n\r\nnot valid json",
        )
        .await;
        assert_eq!(status, 400, "expected 400 for bad JSON");
    }

    #[tokio::test]
    async fn test_messages_empty_model() {
        let addr = spawn_test_server().await;
        let payload =
            r#"{"model":"","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}"#;
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "expected 400 for empty model, body: {body}");
    }

    #[tokio::test]
    async fn test_messages_empty_messages_array() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"claude-sonnet-4-5","max_tokens":100,"messages":[]}"#;
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "expected 400 for empty messages, body: {body}");
    }

    #[tokio::test]
    async fn test_messages_zero_max_tokens() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"claude-sonnet-4-5","max_tokens":0,"messages":[{"role":"user","content":"hi"}]}"#;
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "expected 400 for max_tokens=0, body: {body}");
    }

    #[tokio::test]
    async fn test_messages_excessive_max_tokens() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"claude-sonnet-4-5","max_tokens":999999,"messages":[{"role":"user","content":"hi"}]}"#;
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(
            status, 400,
            "expected 400 for excessive max_tokens, body: {body}"
        );
    }
}
