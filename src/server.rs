use base64::Engine as _;
use http_body_util::{BodyExt, Either, Full};
use hyper::body::{Bytes, Frame};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, mpsc};
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
use crate::format::google::{
    CloudCodeRequest, Content as GoogleContent,
    GenerateContentRequest as GoogleGenerateContentRequest,
    GenerationConfig as GoogleGenerationConfig, Part as GooglePart, TextPart as GoogleTextPart,
};
use crate::format::{
    ChatCompletionRequest, MessagesRequest, ModelInfo, ModelsResponse, StreamEvent,
};
use crate::models::{
    Model, get_fallback_model, get_model_family, is_thinking_model, resolve_model_alias,
    resolve_with_mappings,
};
use crate::stats::get_stats;

/// Maximum time to wait for a single upstream frame before considering the
/// stream stalled (seconds).
const STREAM_FRAME_TIMEOUT_SECS: u64 = 300;

/// Heartbeat cadence for OpenAI/Responses SSE endpoints.
const STREAM_HEARTBEAT_SECS: u64 = 15;

/// Channel buffer size for streaming SSE responses.
///
/// Sized to allow the upstream parser to stay ahead of the client without
/// unbounded memory growth.  Each item is a small SSE text frame.
const STREAM_CHANNEL_BUFFER: usize = 64;

/// Number of log lines sent immediately when a client connects to
/// `/api/logs/stream`.
const LOG_STREAM_TAIL_LINES: usize = 100;

/// Poll interval for checking new log data while streaming.
const LOG_STREAM_POLL_INTERVAL_MS: u64 = 500;

/// SSE heartbeat cadence for `/api/logs/stream`.
#[cfg(not(test))]
const LOG_STREAM_HEARTBEAT_SECS: u64 = 15;
#[cfg(test)]
const LOG_STREAM_HEARTBEAT_SECS: u64 = 1;

/// A streaming response body backed by an `mpsc` channel.
///
/// Each received `Bytes` value is emitted as a single DATA frame.
/// When the sender is dropped the body signals end-of-stream.
pub struct ChannelBody {
    rx: mpsc::Receiver<Bytes>,
}

impl ChannelBody {
    fn new(rx: mpsc::Receiver<Bytes>) -> Self {
        Self { rx }
    }
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            Poll::Ready(None) => Poll::Ready(None), // channel closed = end of stream
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Response body type: either a buffered `Full<Bytes>` (non-streaming) or a
/// channel-backed streaming body.
type ResponseBody = Either<Full<Bytes>, ChannelBody>;

/// Wrap a `Full<Bytes>` into the unified response body type.
fn full_body(body: Full<Bytes>) -> ResponseBody {
    Either::Left(body)
}

/// Create a streaming response body, returning the sender and body.
fn streaming_body() -> (mpsc::Sender<Bytes>, ResponseBody) {
    let (tx, rx) = mpsc::channel(STREAM_CHANNEL_BUFFER);
    (tx, Either::Right(ChannelBody::new(rx)))
}

fn max_request_size() -> usize {
    get_config().server.max_request_size_bytes
}

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
) -> Result<Response<ResponseBody>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Use client-provided X-Request-ID if present, otherwise generate one
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(generate_request_id);

    debug!(
        method = %method,
        path = %path,
        remote = %remote_addr,
        request_id = %request_id,
        "Received request"
    );

    let start = std::time::Instant::now();

    // Handle CORS preflight requests
    if method == Method::OPTIONS {
        return Ok(cors_preflight_response());
    }

    // Check API key authentication for /v1/* and /v1beta/* endpoints
    let config = get_config();
    if (path.starts_with("/v1/") || path.starts_with("/v1beta/"))
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
            return Ok(error_response_for_path(
                StatusCode::UNAUTHORIZED,
                "Invalid or missing API key",
                "authentication_error",
                &path,
                &request_id,
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
            (Method::POST, "/chat/completions") => {
                handle_chat_completions(req, state, &request_id).await
            }

            // OpenAI Responses API (used by Codex CLI)
            (Method::POST, "/v1/responses") => handle_responses(req, state, &request_id).await,
            (Method::POST, "/responses") => handle_responses(req, state, &request_id).await,
            // Compatibility alias for clients that mis-join base paths
            // and emit `/v1/chat/completions/responses`.
            (Method::POST, "/v1/chat/completions/responses") => {
                handle_responses(req, state, &request_id).await
            }
            (Method::POST, "/chat/completions/responses") => {
                handle_responses(req, state, &request_id).await
            }

            // OpenAI legacy Completions compatibility
            (Method::POST, "/v1/completions") => handle_completions(req, state, &request_id).await,
            (Method::POST, "/completions") => handle_completions(req, state, &request_id).await,

            // OpenAI Images API
            (Method::POST, "/v1/images/generations") => {
                handle_images_generations(req, state, &request_id).await
            }
            (Method::POST, "/images/generations") => {
                handle_images_generations(req, state, &request_id).await
            }
            (Method::POST, "/v1/images/edits") => {
                handle_images_edit_like(req, state, &request_id, ImageEditMode::Edits).await
            }
            (Method::POST, "/images/edits") => {
                handle_images_edit_like(req, state, &request_id, ImageEditMode::Edits).await
            }
            (Method::POST, "/v1/images/variations") => {
                handle_images_edit_like(req, state, &request_id, ImageEditMode::Variations).await
            }
            (Method::POST, "/images/variations") => {
                handle_images_edit_like(req, state, &request_id, ImageEditMode::Variations).await
            }

            // OpenAI Audio API
            (Method::POST, "/v1/audio/transcriptions") => {
                handle_audio_transcriptions(req, state, &request_id).await
            }
            (Method::POST, "/audio/transcriptions") => {
                handle_audio_transcriptions(req, state, &request_id).await
            }

            // Token counting API — estimates token count using chars/4 heuristic
            (Method::POST, "/v1/messages/count_tokens") => handle_count_tokens(req).await,

            // Native Gemini API
            (Method::GET, "/v1beta/models") => handle_gemini_models().await,
            (Method::GET, p) if p.starts_with("/v1beta/models/") => handle_gemini_model(p).await,
            (Method::POST, p)
                if p.starts_with("/v1beta/models/") && p.ends_with(":countTokens") =>
            {
                handle_gemini_count_tokens(req, p).await
            }
            (Method::POST, p)
                if p.starts_with("/v1beta/models/") && p.ends_with(":generateContent") =>
            {
                handle_gemini_generate_content(req, state, p, &request_id).await
            }
            (Method::POST, p)
                if p.starts_with("/v1beta/models/") && p.ends_with(":streamGenerateContent") =>
            {
                handle_gemini_stream_generate_content(req, state, p, &request_id).await
            }

            // Internal warmup endpoint
            (Method::POST, "/internal/warmup") => {
                handle_internal_warmup(req, state, &request_id).await
            }

            // Event logging batch (Claude Code sends these - acknowledge silently)
            (Method::POST, "/api/event_logging/batch") => {
                Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
            }
            (Method::POST, "/v1/api/event_logging/batch") => {
                Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
            }
            (Method::POST, "/v1/api/event_logging") => {
                Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
            }

            // Claude Code heartbeat/event requests to root
            (Method::POST, "/") => Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#)),

            // Models API
            (Method::GET, "/v1/models") => handle_models().await,
            (Method::GET, "/models") => handle_models().await,
            (Method::GET, p) if p.starts_with("/v1/models/") => handle_model_by_id(p).await,
            (Method::GET, p) if p.starts_with("/models/") => handle_model_by_id(p).await,
            (Method::POST, "/v1/models/detect") => handle_model_detect(req, &request_id).await,
            (Method::POST, "/models/detect") => handle_model_detect(req, &request_id).await,

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
                    .body(full_body(Full::new(Bytes::from(json))))
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
            (Method::GET, "/health") | (Method::GET, "/healthz") | (Method::GET, "/") => {
                Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
            }

            // 404 for everything else
            _ => Ok(error_response_for_path(
                StatusCode::NOT_FOUND,
                "Not found",
                "not_found",
                &path,
                &request_id,
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

    match response {
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
            } else if is_internal_endpoint(&path) {
                debug!(
                    method = %method,
                    path = %path,
                    status = status,
                    duration_ms = duration.as_millis(),
                    request_id = %request_id,
                    "Request completed"
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
            Ok(resp)
        }
        Err(e) => {
            let resp = error_to_response(&e, &request_id, &path);
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

/// Returns true for internal/monitoring endpoints that should be logged at DEBUG
/// level instead of INFO to avoid filling the log with TUI polling noise.
fn is_internal_endpoint(path: &str) -> bool {
    matches!(
        path,
        "/" | "/health"
            | "/stats"
            | "/v1/stats"
            | "/cache/stats"
            | "/account-limits"
            | "/internal/warmup"
            | "/api/event_logging/batch"
            | "/v1/api/event_logging/batch"
            | "/v1/api/event_logging"
    )
}

fn generate_request_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("Failed to generate random bytes");
    format!(
        "req_{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
    )
}

/// Get access token and project ID using account selection strategy.
///
/// The write lock is held only briefly for account selection and bookkeeping.
/// Token refresh (network I/O) happens outside the lock to avoid blocking
/// concurrent requests.
/// Returns (access_token, project_id, account_id, account_email)
async fn get_account_credentials(
    state: &Arc<ServerState>,
    model: &str,
) -> Result<(String, String, String, String), Error> {
    // Phase 1: Select account and extract data under a brief write lock.
    // If the cached token is still valid we return immediately.
    let (account_id, project_id, email, token_or_refresh) = {
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

        let project_id = account.project_id.clone().unwrap_or_default();
        let id = account.id.clone();
        let email_val = account.email.clone();

        // Update last_used timestamp and consume a token
        account.last_used = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        account.consume_token();

        if account.is_access_token_valid() {
            // Fast path: token is still valid, no network I/O needed.
            let token = account.access_token.clone().unwrap();
            (id, project_id, email_val, Ok(token))
        } else {
            // Slow path: need to refresh. Clone the refresh token and release the lock.
            let refresh_token = account.refresh_token.clone();
            (id, project_id, email_val, Err(refresh_token))
        }
        // Write lock is dropped here.
    };

    let access_token = match token_or_refresh {
        Ok(token) => token,
        Err(refresh_token) => {
            // Phase 2: Refresh token outside the lock (network I/O).
            let (new_token, expires_in) =
                crate::auth::token::refresh_access_token(&state.http_client, &refresh_token)
                    .await?;

            // Phase 3: Store the refreshed token under a brief write lock.
            {
                let mut accounts = state.accounts.write().await;
                if let Some(account) = accounts.get_account_mut(&account_id) {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    account.access_token = Some(new_token.clone());
                    account.access_token_expires = Some(now + expires_in);
                }
            }

            new_token
        }
    };

    debug!(
        model = %model,
        account_id = %&account_id[..8.min(account_id.len())],
        project_id = %project_id,
        "Using account credentials"
    );

    Ok((access_token, project_id, account_id, email))
}

/// Record request outcome for an account.
///
/// File I/O (account state persistence) is offloaded to a blocking task so the
/// write lock is only held for in-memory bookkeeping and serialization.
async fn record_request_outcome(
    state: &Arc<ServerState>,
    account_id: &str,
    model: &str,
    success: bool,
    rate_limit_until: Option<u64>,
) {
    // Serialize under the lock, then write to disk outside the lock.
    let save_data = {
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

        // Only serialize if we need to persist (failure or rate limit)
        if !success || rate_limit_until.is_some() {
            serde_json::to_string_pretty(&*accounts)
                .ok()
                .map(|json| (crate::auth::accounts::AccountStore::path(), json))
        } else {
            None
        }
        // Write lock is dropped here.
    };

    // Write to disk outside the lock using a blocking task.
    if let Some((path, json)) = save_data {
        let dir = path.parent().map(|p| p.to_path_buf());
        tokio::task::spawn_blocking(move || {
            if let Some(dir) = dir {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(error = %e, "Failed to save account state");
            }
        });
    }
}

/// Extract outcome from a request result, log it, and record it for account health tracking.
async fn track_request_outcome(
    state: &Arc<ServerState>,
    account_id: &str,
    account_email: &str,
    model: &str,
    request_id: &str,
    result: &Result<Response<ResponseBody>, Error>,
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

/// Record token usage from a completed response
fn record_usage(model: &str, usage: &crate::format::anthropic::Usage) {
    get_stats().record_token_usage(
        model,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens.unwrap_or(0),
    );
}

async fn handle_messages(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
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

    let max_request_size = max_request_size();

    if let Some(len) = req.headers().get("content-length")
        && let Ok(len_str) = len.to_str()
        && let Ok(len) = len_str.parse::<usize>()
        && len > max_request_size
    {
        return Err(Error::Api(ApiError::RequestTooLarge {
            size: len,
            max: max_request_size,
        }));
    }

    let body_bytes = read_body_limited(req.into_body(), max_request_size).await?;

    let mut messages_request: MessagesRequest = serde_json::from_slice(&body_bytes)?;

    // Resolve model aliases (e.g., "opus" -> "claude-opus-4-6-thinking")
    let original_model = messages_request.model.clone();
    let config = get_config();
    messages_request.model = resolve_with_mappings(
        &messages_request.model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );

    debug!(
        original_model = %original_model,
        resolved_model = %messages_request.model,
        request_id = %request_id,
        "Model resolution"
    );

    validate_request(&messages_request)?;

    if config.server.warmup_intercept_enabled
        && is_warmup_request(
            &messages_request,
            config.server.warmup_intercept_max_text_len,
        )
    {
        info!(
            model = %messages_request.model,
            request_id = %request_id,
            "Warmup request intercepted"
        );
        get_stats().record_request(&messages_request.model, "/v1/messages");
        return build_warmup_intercept_response(&messages_request, request_id);
    }

    // Try the primary model first
    let result =
        execute_messages_request(&messages_request, &state, request_id, false, bypass_cache).await;

    // Check if fallback is enabled and we got a quota exhaustion error
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
) -> Result<Response<ResponseBody>, Error> {
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
            messages_request.max_tokens,
            messages_request.top_p,
            messages_request.top_k,
            messages_request
                .stop_sequences
                .as_ref()
                .map(|s| serde_json::to_string(s).unwrap_or_default())
                .as_deref(),
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
    let request_body = Bytes::from(serde_json::to_vec(&cc_request)?);

    // Thinking models must use streaming endpoint even for non-streaming requests
    // (the non-streaming generateContent endpoint returns 429 for thinking models)
    let is_thinking = is_thinking_model(model);

    let result = if is_streaming {
        handle_streaming_messages(
            &state.cloudcode_client,
            request_body.clone(),
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else if is_thinking {
        // Use streaming endpoint but return non-streaming response
        handle_thinking_non_streaming_messages(
            &state.cloudcode_client,
            request_body.clone(),
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else {
        handle_non_streaming_messages(
            &state.cloudcode_client,
            request_body.clone(),
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

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum LegacyPrompt {
    Single(String),
    Multiple(Vec<String>),
}

impl LegacyPrompt {
    fn into_text(self) -> String {
        match self {
            LegacyPrompt::Single(text) => text,
            LegacyPrompt::Multiple(parts) => parts.join("\n"),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyCompletionRequest {
    model: String,
    prompt: LegacyPrompt,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<crate::format::openai::StopSequence>,
    #[serde(default)]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct LegacyCompletionResponse {
    id: String,
    object: String,
    created: i64,
    model: String,
    choices: Vec<LegacyCompletionChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<LegacyCompletionUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_fingerprint: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct LegacyCompletionChoice {
    text: String,
    index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logprobs: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct LegacyCompletionUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
struct LegacyCompletionStreamChunk {
    id: String,
    object: String,
    created: i64,
    model: String,
    choices: Vec<LegacyCompletionChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<LegacyCompletionUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_fingerprint: Option<String>,
}

fn convert_chat_chunk_to_legacy_chunk(
    chunk: crate::format::openai::ChatCompletionChunk,
) -> LegacyCompletionStreamChunk {
    LegacyCompletionStreamChunk {
        id: chunk.id,
        object: "text_completion.chunk".to_string(),
        created: chunk.created,
        model: chunk.model,
        choices: chunk
            .choices
            .into_iter()
            .map(|choice| LegacyCompletionChoice {
                text: choice.delta.content.unwrap_or_default(),
                index: choice.index,
                finish_reason: choice.finish_reason,
                logprobs: choice.logprobs,
            })
            .collect(),
        usage: chunk.usage.map(|usage| LegacyCompletionUsage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }),
        system_fingerprint: chunk.system_fingerprint,
    }
}

async fn adapt_chat_stream_to_legacy_completions(
    response: Response<ResponseBody>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let (tx, body) = streaming_body();
    let mut incoming = response.into_body();

    tokio::spawn(async move {
        use crate::format::openai::ChatCompletionChunk;
        use http_body_util::BodyExt;

        let mut buffer = String::new();

        let emit_comment = |tx: &mpsc::Sender<Bytes>, event: &str| -> bool {
            tx.try_send(Bytes::from(format!("{event}\n\n"))).is_ok()
        };
        let emit_data = |tx: &mpsc::Sender<Bytes>, payload: &str| -> bool {
            tx.try_send(Bytes::from(format!("data: {payload}\n\n")))
                .is_ok()
        };

        let process_event = |event: &str, tx: &mpsc::Sender<Bytes>| -> bool {
            if event.trim().is_empty() {
                return true;
            }

            // Pass through SSE comments (heartbeats) unchanged.
            if event.lines().all(|line| line.starts_with(':')) {
                return emit_comment(tx, event);
            }

            let data_payload = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|payload| payload.trim_start())
                .collect::<Vec<_>>()
                .join("\n");

            if data_payload.is_empty() {
                return true;
            }

            if data_payload == "[DONE]" {
                return emit_data(tx, "[DONE]");
            }

            let chat_chunk: ChatCompletionChunk = match serde_json::from_str(&data_payload) {
                Ok(chunk) => chunk,
                Err(_) => {
                    // Unknown payload shape - forward as-is.
                    return emit_data(tx, &data_payload);
                }
            };

            let legacy_chunk = convert_chat_chunk_to_legacy_chunk(chat_chunk);
            let payload = serde_json::to_string(&legacy_chunk).unwrap_or_default();
            emit_data(tx, &payload)
        };

        while let Some(next_frame) = incoming.frame().await {
            match next_frame {
                Ok(frame) => {
                    if let Ok(data) = frame.into_data() {
                        buffer.push_str(&String::from_utf8_lossy(&data));
                        while let Some(split_at) = buffer.find("\n\n") {
                            let event = buffer[..split_at].to_string();
                            buffer.drain(..split_at + 2);
                            if !process_event(&event, &tx) {
                                return;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }

        if !buffer.trim().is_empty() {
            let _ = process_event(buffer.trim_end_matches('\n'), &tx);
        }
    });

    Ok(sse_streaming_response(body, request_id))
}

async fn handle_chat_completions(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
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

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;

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

    handle_chat_completion_request(chat_request, state, request_id, "/v1/chat/completions").await
}

async fn handle_completions(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    use crate::format::openai::{ChatCompletionResponse, ChatContent, ChatMessage};

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

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let json_value: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {}", e),
                "invalid_request_error",
            ));
        }
    };

    // Support both legacy prompt payloads and chat-style messages payloads on
    // `/v1/completions` for broad client compatibility.
    if json_value.get("messages").is_some() {
        let chat_request: ChatCompletionRequest = match serde_json::from_value(json_value) {
            Ok(r) => r,
            Err(e) => {
                return Ok(openai_error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid JSON: {}", e),
                    "invalid_request_error",
                ));
            }
        };
        return handle_chat_completion_request(chat_request, state, request_id, "/v1/completions")
            .await;
    }

    let legacy_request: LegacyCompletionRequest = match serde_json::from_value(json_value) {
        Ok(r) => r,
        Err(e) => {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {}", e),
                "invalid_request_error",
            ));
        }
    };

    if legacy_request.n.unwrap_or(1) > 1 {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "n > 1 is not supported",
            "invalid_request_error",
        ));
    }

    let prompt_text = legacy_request.prompt.into_text();
    if prompt_text.trim().is_empty() {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "prompt cannot be empty",
            "invalid_request_error",
        ));
    }

    let chat_request = ChatCompletionRequest {
        model: legacy_request.model,
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(ChatContent::Text(prompt_text)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        max_tokens: legacy_request.max_tokens,
        max_completion_tokens: legacy_request.max_completion_tokens,
        temperature: legacy_request.temperature,
        top_p: legacy_request.top_p,
        stop: legacy_request.stop,
        stream: legacy_request.stream,
        tools: None,
        tool_choice: None,
        n: legacy_request.n,
        user: legacy_request.user,
        response_format: None,
    };

    let response =
        handle_chat_completion_request(chat_request, state, request_id, "/v1/completions").await?;
    if !response.status().is_success() {
        return Ok(response);
    }

    if legacy_request.stream {
        return adapt_chat_stream_to_legacy_completions(response, request_id).await;
    }

    let cache_status = response
        .headers()
        .get("x-cache-status")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let collected = match response.into_body().collect().await {
        Ok(c) => c,
        Err(e) => {
            return Ok(openai_error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to read completion response: {}", e),
                "api_error",
            ));
        }
    };
    let chat_response: ChatCompletionResponse = match serde_json::from_slice(&collected.to_bytes())
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(openai_error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to convert completion response: {}", e),
                "api_error",
            ));
        }
    };

    let legacy_response = LegacyCompletionResponse {
        id: chat_response.id,
        object: "text_completion".to_string(),
        created: chat_response.created,
        model: chat_response.model,
        choices: chat_response
            .choices
            .into_iter()
            .map(|choice| LegacyCompletionChoice {
                text: choice.message.content.unwrap_or_default(),
                index: choice.index,
                finish_reason: choice.finish_reason,
                logprobs: choice.logprobs,
            })
            .collect(),
        usage: chat_response.usage.map(|usage| LegacyCompletionUsage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }),
        system_fingerprint: chat_response.system_fingerprint,
    };
    let body = serde_json::to_vec(&legacy_response)?;

    Ok(json_ok_response(body, request_id, cache_status.as_deref()))
}

async fn handle_chat_completion_request(
    chat_request: ChatCompletionRequest,
    state: Arc<ServerState>,
    request_id: &str,
    stats_route: &'static str,
) -> Result<Response<ResponseBody>, Error> {
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
    let config = get_config();
    messages_request.model = resolve_with_mappings(
        &messages_request.model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );

    debug!(
        original_model = %original_model,
        resolved_model = %messages_request.model,
        request_id = %request_id,
        "Model resolution (OpenAI)"
    );

    validate_request(&messages_request)?;

    // Try the primary model first
    let result =
        execute_openai_request(&messages_request, &state, request_id, false, stats_route).await;

    // Check if fallback is enabled and we got a quota exhaustion error
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

        return execute_openai_request(&fallback_request, &state, request_id, true, stats_route)
            .await;
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
    stats_route: &'static str,
) -> Result<Response<ResponseBody>, Error> {
    let is_streaming = messages_request.stream;
    let model = &messages_request.model;

    get_stats().record_request(model, stats_route);

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
    let request_body = Bytes::from(serde_json::to_vec(&cc_request)?);

    let is_thinking = is_thinking_model(model);

    let result = if is_streaming {
        handle_openai_streaming(
            &state.cloudcode_client,
            request_body.clone(),
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else if is_thinking {
        handle_openai_thinking_non_streaming(
            &state.cloudcode_client,
            request_body.clone(),
            &access_token,
            model,
            &cc_request.request_id,
        )
        .await
    } else {
        handle_openai_non_streaming(
            &state.cloudcode_client,
            request_body.clone(),
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
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let response = client.send_request(body, access_token, model).await?;
    let anthropic_response = parse_response(&response, model, request_id);
    record_usage(model, &anthropic_response.usage);

    let openai_response =
        crate::format::anthropic_to_openai(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "OpenAI response", &openai_response);

    let body = serde_json::to_vec(&openai_response)?;
    Ok(json_ok_response(body, request_id, None))
}

async fn handle_openai_thinking_non_streaming(
    client: &CloudCodeClient,
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let (events, _body_bytes) = collect_sse_events(client, body, access_token, model).await?;

    check_stream_errors(
        &events,
        model,
        request_id,
        " (OpenAI thinking non-streaming)",
    )?;

    let anthropic_response = crate::format::build_response_from_events(&events, model, request_id);
    record_usage(model, &anthropic_response.usage);
    let openai_response =
        crate::format::anthropic_to_openai(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "OpenAI response", &openai_response);

    let response_body = serde_json::to_vec(&openai_response)?;
    Ok(json_ok_response(response_body, request_id, Some("BYPASS")))
}

/// Handle OpenAI-format streaming with true SSE pass-through.
///
/// Each upstream Anthropic-format event is converted to an OpenAI
/// `chat.completion.chunk` and forwarded through the channel immediately.
async fn handle_openai_streaming(
    client: &CloudCodeClient,
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let upstream = client
        .send_streaming_request(body, access_token, model)
        .await?;

    let (tx, body) = streaming_body();
    let response = sse_streaming_response(body, request_id);

    let model = model.to_string();
    let request_id = request_id.to_string();

    tokio::spawn(async move {
        use crate::format::openai::{
            ChatCompletionChunk, ChatUsage, ChunkChoice, ChunkDelta, ChunkFunction, ChunkToolCall,
        };
        use std::time::{SystemTime, UNIX_EPOCH};

        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let chunk_id = format!("chatcmpl-{}", request_id);

        let mut parser = SseParser::new(&model);
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;
        let mut sent_role = false;
        let mut tool_call_index = 0u32;

        // Helper closure: serialize and send a chunk
        let send_chunk = |tx: &mpsc::Sender<Bytes>, chunk: &ChatCompletionChunk| -> bool {
            let data = format!(
                "data: {}\n\n",
                serde_json::to_string(chunk).unwrap_or_default()
            );
            tx.try_send(Bytes::from(data)).is_ok()
        };

        let process_event = |event: &StreamEvent,
                             tx: &mpsc::Sender<Bytes>,
                             input_tokens: &mut u32,
                             output_tokens: &mut u32,
                             sent_role: &mut bool,
                             tool_call_index: &mut u32| {
            match event {
                StreamEvent::MessageStart { message } => {
                    *input_tokens = message.usage.input_tokens;
                    let chunk = ChatCompletionChunk {
                        id: chunk_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model.clone(),
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
                    send_chunk(tx, &chunk);
                    *sent_role = true;
                }
                StreamEvent::ContentBlockStart {
                    content_block: crate::format::ContentBlock::ToolUse { id, name, .. },
                    index: _,
                } => {
                    // Emit initial tool call chunk with name and id
                    let chunk = ChatCompletionChunk {
                        id: chunk_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChunkDelta {
                                role: if !*sent_role {
                                    Some("assistant".to_string())
                                } else {
                                    None
                                },
                                content: None,
                                tool_calls: Some(vec![ChunkToolCall {
                                    index: *tool_call_index,
                                    id: Some(id.clone()),
                                    call_type: Some("function".to_string()),
                                    function: Some(ChunkFunction {
                                        name: Some(name.clone()),
                                        arguments: None,
                                    }),
                                }]),
                            },
                            finish_reason: None,
                            logprobs: None,
                        }],
                        usage: None,
                        system_fingerprint: None,
                    };
                    send_chunk(tx, &chunk);
                    *sent_role = true;
                }
                StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                    crate::format::ContentDelta::Text { text } => {
                        let chunk = ChatCompletionChunk {
                            id: chunk_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created,
                            model: model.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChunkDelta {
                                    role: if !*sent_role {
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
                        send_chunk(tx, &chunk);
                        *sent_role = true;
                    }
                    crate::format::ContentDelta::Thinking { thinking } => {
                        let chunk = ChatCompletionChunk {
                            id: chunk_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created,
                            model: model.clone(),
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
                        send_chunk(tx, &chunk);
                    }
                    crate::format::ContentDelta::InputJson { partial_json } => {
                        // Stream tool call argument deltas
                        let chunk = ChatCompletionChunk {
                            id: chunk_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created,
                            model: model.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChunkDelta {
                                    role: None,
                                    content: None,
                                    tool_calls: Some(vec![ChunkToolCall {
                                        index: *tool_call_index,
                                        id: None,
                                        call_type: None,
                                        function: Some(ChunkFunction {
                                            name: None,
                                            arguments: Some(partial_json.clone()),
                                        }),
                                    }]),
                                },
                                finish_reason: None,
                                logprobs: None,
                            }],
                            usage: None,
                            system_fingerprint: None,
                        };
                        send_chunk(tx, &chunk);
                    }
                    _ => {}
                },
                StreamEvent::ContentBlockStop { .. } => {
                    // If finishing a tool call block, increment tool call index
                    // for next potential tool call
                    *tool_call_index += 1;
                }
                StreamEvent::MessageDelta { delta, usage } => {
                    *output_tokens = usage.output_tokens;
                    let finish_reason = delta.stop_reason.map(|r| r.to_openai_str().to_string());
                    let chunk = ChatCompletionChunk {
                        id: chunk_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model.clone(),
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
                            prompt_tokens: *input_tokens,
                            completion_tokens: *output_tokens,
                            total_tokens: *input_tokens + *output_tokens,
                        }),
                        system_fingerprint: None,
                    };
                    send_chunk(tx, &chunk);
                }
                _ => {}
            }
        };

        let mut incoming = upstream.into_body();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(STREAM_HEARTBEAT_SECS));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let _ = tx.send(Bytes::from(": ping\n\n")).await;
        heartbeat.tick().await;

        let mut frame_timeout = Box::pin(tokio::time::sleep(Duration::from_secs(
            STREAM_FRAME_TIMEOUT_SECS,
        )));

        loop {
            use http_body_util::BodyExt;
            tokio::select! {
                _ = &mut frame_timeout => {
                    warn!("Upstream frame timeout in OpenAI streaming");
                    break;
                }
                _ = heartbeat.tick() => {
                    if tx.send(Bytes::from(": ping\n\n")).await.is_err() {
                        break;
                    }
                }
                next_frame = incoming.frame() => {
                    match next_frame {
                        Some(Ok(frame)) => {
                            frame_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(STREAM_FRAME_TIMEOUT_SECS));
                            if let Ok(data) = frame.into_data() {
                                let chunk_str = String::from_utf8_lossy(&data);
                                for event in parser.feed(&chunk_str) {
                                    process_event(
                                        &event,
                                        &tx,
                                        &mut input_tokens,
                                        &mut output_tokens,
                                        &mut sent_role,
                                        &mut tool_call_index,
                                    );
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "Error reading upstream for OpenAI streaming");
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        for event in parser.finish() {
            process_event(
                &event,
                &tx,
                &mut input_tokens,
                &mut output_tokens,
                &mut sent_role,
                &mut tool_call_index,
            );
        }

        get_stats().record_token_usage(&model, input_tokens, output_tokens, 0);
        let _ = tx.send(Bytes::from("data: [DONE]\n\n")).await;
    });

    Ok(response)
}

// ============================================================================
// OpenAI Responses API handlers (used by Codex CLI)
// ============================================================================

async fn handle_responses(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
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

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;

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
    let config = get_config();
    messages_request.model = resolve_with_mappings(
        &messages_request.model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );

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
    let request_body = Bytes::from(serde_json::to_vec(&cc_request)?);

    // Thinking models must use streaming endpoint even for non-streaming requests
    let is_thinking = is_thinking_model(model);

    let result = if is_streaming {
        handle_responses_streaming(
            &state.cloudcode_client,
            request_body.clone(),
            &access_token,
            model,
            request_id,
        )
        .await
    } else if is_thinking {
        // Use streaming endpoint but return non-streaming response
        handle_responses_thinking_non_streaming(
            &state.cloudcode_client,
            request_body.clone(),
            &access_token,
            model,
            request_id,
        )
        .await
    } else {
        handle_responses_non_streaming(
            &state.cloudcode_client,
            request_body.clone(),
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
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let response = client.send_request(body, access_token, model).await?;
    let anthropic_response = parse_response(&response, model, request_id);
    record_usage(model, &anthropic_response.usage);

    let responses_response =
        crate::format::anthropic_to_responses(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "Responses API response", &responses_response);

    let body = serde_json::to_vec(&responses_response)?;
    Ok(json_ok_response(body, request_id, None))
}

// Thinking models must use streaming endpoint but return non-streaming response
async fn handle_responses_thinking_non_streaming(
    client: &CloudCodeClient,
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let (all_events, _body_bytes) = collect_sse_events(client, body, access_token, model).await?;

    check_stream_errors(
        &all_events,
        model,
        request_id,
        " (Responses thinking non-streaming)",
    )?;

    let anthropic_response =
        crate::format::build_response_from_events(&all_events, model, request_id);
    record_usage(model, &anthropic_response.usage);

    let responses_response =
        crate::format::anthropic_to_responses(&anthropic_response, model, request_id);

    log_if_enabled(request_id, "Responses API response", &responses_response);

    let body = serde_json::to_vec(&responses_response)?;
    Ok(json_ok_response(body, request_id, None))
}

/// Handle Responses API streaming with true SSE pass-through.
///
/// Converts upstream Anthropic-format stream events to OpenAI Responses API
/// streaming events and forwards them through a channel as they arrive.
async fn handle_responses_streaming(
    client: &CloudCodeClient,
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let upstream = client
        .send_streaming_request(body, access_token, model)
        .await?;

    let (tx, body) = streaming_body();
    let response = sse_streaming_response(body, request_id);

    let model = model.to_string();
    let request_id = request_id.to_string();

    tokio::spawn(async move {
        use crate::format::responses::{
            InputTokensDetails, OutputTokensDetails, ResponseOutputContent, ResponseOutputItem,
            ResponseStreamEvent, ResponseUsage, ResponsesResponse,
        };
        use std::time::{SystemTime, UNIX_EPOCH};

        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let resp_id = format!("resp_{}", request_id);

        let mut parser = SseParser::new(&model);
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

        let mut tool_calls: Vec<(String, String, String)> = vec![];
        let mut current_tool_json = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();

        // Helper: send a Responses API SSE event through the channel.
        let emit = |tx: &mpsc::Sender<Bytes>, event: &ResponseStreamEvent| {
            let data = format!(
                "data: {}\n\n",
                serde_json::to_string(event).unwrap_or_default()
            );
            let _ = tx.try_send(Bytes::from(data));
        };

        let make_response = |status: &'static str,
                             out: Vec<ResponseOutputItem>,
                             usage: Option<ResponseUsage>|
         -> ResponsesResponse {
            ResponsesResponse {
                id: resp_id.clone(),
                object: "response",
                created_at,
                model: model.clone(),
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

        // ---- Process events from upstream ----
        let process_event = |event: &StreamEvent,
                             tx: &mpsc::Sender<Bytes>,
                             input_tokens: &mut u32,
                             output_tokens: &mut u32,
                             cache_read_tokens: &mut u32,
                             reasoning_tokens: &mut u32,
                             text_content: &mut String,
                             reasoning_content: &mut String,
                             sent_initial: &mut bool,
                             message_added: &mut bool,
                             output_index: &mut usize,
                             tool_calls: &mut Vec<(String, String, String)>,
                             current_tool_json: &mut String,
                             current_tool_id: &mut String,
                             current_tool_name: &mut String| {
            match event {
                StreamEvent::MessageStart { message } => {
                    *input_tokens = message.usage.input_tokens;
                    *cache_read_tokens = message.usage.cache_read_input_tokens.unwrap_or(0);
                    if !*sent_initial {
                        emit(
                            tx,
                            &ResponseStreamEvent::ResponseCreated {
                                response: Box::new(make_response("in_progress", vec![], None)),
                            },
                        );
                        *sent_initial = true;
                    }
                }
                StreamEvent::ContentBlockStart {
                    content_block,
                    index: _,
                } => match content_block {
                    crate::format::ContentBlock::Text { .. } => {
                        if !*message_added {
                            let msg_item = ResponseOutputItem::Message {
                                id: format!("msg_{}", &request_id[..8.min(request_id.len())]),
                                role: "assistant",
                                status: "in_progress",
                                content: vec![],
                            };
                            emit(
                                tx,
                                &ResponseStreamEvent::OutputItemAdded {
                                    output_index: *output_index,
                                    item: msg_item,
                                },
                            );
                            let part = ResponseOutputContent::OutputText {
                                text: String::new(),
                                annotations: vec![],
                            };
                            emit(
                                tx,
                                &ResponseStreamEvent::ContentPartAdded {
                                    output_index: *output_index,
                                    content_index,
                                    part,
                                },
                            );
                            *message_added = true;
                        }
                    }
                    crate::format::ContentBlock::ToolUse { id, name, .. } => {
                        *current_tool_id = id.clone();
                        *current_tool_name = name.clone();
                        current_tool_json.clear();
                        let fc_item = ResponseOutputItem::FunctionCall {
                            id: format!("fc_{}", id),
                            call_id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                            status: "in_progress",
                        };
                        emit(
                            tx,
                            &ResponseStreamEvent::OutputItemAdded {
                                output_index: *output_index,
                                item: fc_item,
                            },
                        );
                        *output_index += 1;
                    }
                    _ => {}
                },
                StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                    crate::format::ContentDelta::Text { text } => {
                        text_content.push_str(text);
                        emit(
                            tx,
                            &ResponseStreamEvent::OutputTextDelta {
                                output_index: *output_index,
                                content_index,
                                delta: text.clone(),
                            },
                        );
                    }
                    crate::format::ContentDelta::Thinking { thinking } => {
                        reasoning_content.push_str(thinking);
                        *reasoning_tokens += 1;
                    }
                    crate::format::ContentDelta::InputJson { partial_json } => {
                        current_tool_json.push_str(partial_json);
                        // Emit function_call_arguments.delta for streaming tool calls
                        emit(
                            tx,
                            &ResponseStreamEvent::FunctionCallArgumentsDelta {
                                output_index: output_index.saturating_sub(1),
                                delta: partial_json.clone(),
                            },
                        );
                    }
                    _ => {}
                },
                StreamEvent::ContentBlockStop { .. } => {
                    if !current_tool_id.is_empty() {
                        // Emit function_call_arguments.done
                        emit(
                            tx,
                            &ResponseStreamEvent::FunctionCallArgumentsDone {
                                output_index: output_index.saturating_sub(1),
                                arguments: current_tool_json.clone(),
                            },
                        );
                        let fc_item = ResponseOutputItem::FunctionCall {
                            id: format!("fc_{}", current_tool_id),
                            call_id: current_tool_id.clone(),
                            name: current_tool_name.clone(),
                            arguments: current_tool_json.clone(),
                            status: "completed",
                        };
                        emit(
                            tx,
                            &ResponseStreamEvent::OutputItemDone {
                                output_index: output_index.saturating_sub(1),
                                item: fc_item,
                            },
                        );
                        tool_calls.push((
                            std::mem::take(current_tool_id),
                            std::mem::take(current_tool_name),
                            std::mem::take(current_tool_json),
                        ));
                    }
                }
                StreamEvent::MessageDelta { usage, .. } => {
                    *output_tokens = usage.output_tokens;
                }
                _ => {}
            }
        };

        let mut incoming = upstream.into_body();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(STREAM_HEARTBEAT_SECS));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let _ = tx.send(Bytes::from(": ping\n\n")).await;
        heartbeat.tick().await;

        let mut frame_timeout = Box::pin(tokio::time::sleep(Duration::from_secs(
            STREAM_FRAME_TIMEOUT_SECS,
        )));

        loop {
            use http_body_util::BodyExt;
            tokio::select! {
                _ = &mut frame_timeout => {
                    warn!("Upstream frame timeout in Responses streaming");
                    break;
                }
                _ = heartbeat.tick() => {
                    if tx.send(Bytes::from(": ping\n\n")).await.is_err() {
                        break;
                    }
                }
                next_frame = incoming.frame() => {
                    match next_frame {
                        Some(Ok(frame)) => {
                            frame_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(STREAM_FRAME_TIMEOUT_SECS));
                            if let Ok(data) = frame.into_data() {
                                let chunk_str = String::from_utf8_lossy(&data);
                                for event in parser.feed(&chunk_str) {
                                    process_event(
                                        &event,
                                        &tx,
                                        &mut input_tokens,
                                        &mut output_tokens,
                                        &mut cache_read_tokens,
                                        &mut reasoning_tokens,
                                        &mut text_content,
                                        &mut reasoning_content,
                                        &mut sent_initial,
                                        &mut message_added,
                                        &mut output_index,
                                        &mut tool_calls,
                                        &mut current_tool_json,
                                        &mut current_tool_id,
                                        &mut current_tool_name,
                                    );
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "Error reading upstream for Responses streaming");
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        for event in parser.finish() {
            process_event(
                &event,
                &tx,
                &mut input_tokens,
                &mut output_tokens,
                &mut cache_read_tokens,
                &mut reasoning_tokens,
                &mut text_content,
                &mut reasoning_content,
                &mut sent_initial,
                &mut message_added,
                &mut output_index,
                &mut tool_calls,
                &mut current_tool_json,
                &mut current_tool_id,
                &mut current_tool_name,
            );
        }

        // ---- Emit final events ----
        if message_added {
            emit(
                &tx,
                &ResponseStreamEvent::OutputTextDone {
                    output_index,
                    content_index,
                    text: text_content.clone(),
                },
            );
            let part = ResponseOutputContent::OutputText {
                text: text_content.clone(),
                annotations: vec![],
            };
            emit(
                &tx,
                &ResponseStreamEvent::ContentPartDone {
                    output_index,
                    content_index,
                    part: part.clone(),
                },
            );
            let msg_item = ResponseOutputItem::Message {
                id: format!("msg_{}", &request_id[..8.min(request_id.len())]),
                role: "assistant",
                status: "completed",
                content: vec![part],
            };
            emit(
                &tx,
                &ResponseStreamEvent::OutputItemDone {
                    output_index,
                    item: msg_item,
                },
            );
        }

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

        let mut final_output = vec![];
        for (id, name, arguments) in tool_calls {
            final_output.push(ResponseOutputItem::FunctionCall {
                id: format!("fc_{}", &id),
                call_id: id,
                name,
                arguments,
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

        get_stats().record_token_usage(&model, input_tokens, output_tokens, cache_read_tokens);

        emit(
            &tx,
            &ResponseStreamEvent::ResponseCompleted {
                response: Box::new(make_response("completed", final_output, usage)),
            },
        );
        let _ = tx.send(Bytes::from("data: [DONE]\n\n")).await;
    });

    Ok(response)
}

/// Error response format
#[derive(Clone, Copy)]
enum ErrorFormat {
    /// Anthropic format: type:error + error:{type,message} + request_id
    Anthropic,
    /// OpenAI format: error.{message, type, param: null, code: null}
    OpenAI,
    /// Responses format: error.{message, type, code: type}
    Responses,
}

fn error_response_body(
    message: &str,
    error_type: &str,
    format: ErrorFormat,
    request_id: Option<&str>,
) -> String {
    let body = match format {
        ErrorFormat::Anthropic => {
            let mut body = serde_json::json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message
                }
            });
            if let Some(req_id) = request_id {
                body["request_id"] = serde_json::json!(req_id);
            }
            body
        }
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
    };
    body.to_string()
}

fn error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
    format: ErrorFormat,
) -> Response<ResponseBody> {
    let body = error_response_body(message, error_type, format, None);

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(full_body(Full::new(Bytes::from(body))))
        .expect("Response construction with valid headers should not fail")
}

fn error_response_for_path(
    status: StatusCode,
    message: &str,
    error_type: &str,
    path: &str,
    request_id: &str,
) -> Response<ResponseBody> {
    let format = error_format_for_path(path);
    let body = error_response_body(message, error_type, format, Some(request_id));

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("X-Request-Id", request_id)
        .body(full_body(Full::new(Bytes::from(body))))
        .expect("Response construction with valid headers should not fail")
}

fn error_format_for_path(path: &str) -> ErrorFormat {
    if matches!(
        path,
        "/v1/responses"
            | "/responses"
            | "/v1/chat/completions/responses"
            | "/chat/completions/responses"
    ) {
        ErrorFormat::Responses
    } else if path.starts_with("/v1/models/")
        || path.starts_with("/models/")
        || matches!(
            path,
            "/v1/chat/completions"
                | "/chat/completions"
                | "/v1/completions"
                | "/completions"
                | "/v1/models"
                | "/models"
                | "/v1/models/detect"
                | "/models/detect"
                | "/v1/images/generations"
                | "/images/generations"
                | "/v1/images/edits"
                | "/images/edits"
                | "/v1/images/variations"
                | "/images/variations"
                | "/v1/audio/transcriptions"
                | "/audio/transcriptions"
        )
    {
        ErrorFormat::OpenAI
    } else {
        ErrorFormat::Anthropic
    }
}

fn responses_error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
) -> Response<ResponseBody> {
    error_response(status, message, error_type, ErrorFormat::Responses)
}

fn openai_error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
) -> Response<ResponseBody> {
    error_response(status, message, error_type, ErrorFormat::OpenAI)
}

fn is_warmup_text_candidate(text: &str, max_text_len: usize) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > max_text_len {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    lower == "warmup"
        || lower.starts_with("warmup ")
        || lower.starts_with("warmup\n")
        || lower.starts_with("warmup\t")
}

fn tool_result_contains_warmup(
    content: &crate::format::anthropic::ToolResultContent,
    max_text_len: usize,
) -> bool {
    match content {
        crate::format::anthropic::ToolResultContent::Text(text) => {
            is_warmup_text_candidate(text, max_text_len)
        }
        crate::format::anthropic::ToolResultContent::Blocks(blocks) => {
            blocks.iter().any(|block| match block {
                crate::format::ContentBlock::Text { text, .. } => {
                    is_warmup_text_candidate(text, max_text_len)
                }
                _ => false,
            })
        }
    }
}

fn is_warmup_request(request: &MessagesRequest, max_text_len: usize) -> bool {
    if max_text_len == 0 {
        return false;
    }

    let Some(last_message) = request.messages.last() else {
        return false;
    };
    if last_message.role != crate::format::Role::User {
        return false;
    }

    match &last_message.content {
        crate::format::anthropic::MessageContent::Text(text) => {
            is_warmup_text_candidate(text, max_text_len)
        }
        crate::format::anthropic::MessageContent::Blocks(blocks) => {
            blocks.iter().any(|block| match block {
                crate::format::ContentBlock::Text { text, .. } => {
                    is_warmup_text_candidate(text, max_text_len)
                }
                crate::format::ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    is_error.unwrap_or(false) && tool_result_contains_warmup(content, max_text_len)
                }
                _ => false,
            })
        }
    }
}

fn build_warmup_intercept_response(
    request: &MessagesRequest,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let message_id = format!("msg_warmup_{}", chrono::Utc::now().timestamp_millis());

    let mut response = if request.stream {
        let events = [
            crate::format::StreamEvent::MessageStart {
                message: Box::new(crate::format::MessageStart {
                    id: message_id.clone(),
                    message_type: "message".to_string(),
                    role: crate::format::Role::Assistant,
                    content: vec![],
                    model: request.model.clone(),
                    stop_reason: None,
                    stop_sequence: None,
                    usage: crate::format::Usage {
                        input_tokens: 1,
                        output_tokens: 0,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                }),
            },
            crate::format::StreamEvent::ContentBlockStart {
                index: 0,
                content_block: crate::format::ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
            },
            crate::format::StreamEvent::ContentBlockDelta {
                index: 0,
                delta: crate::format::ContentDelta::Text {
                    text: "OK".to_string(),
                },
            },
            crate::format::StreamEvent::ContentBlockStop { index: 0 },
            crate::format::StreamEvent::MessageDelta {
                delta: crate::format::MessageDeltaData {
                    stop_reason: Some(crate::format::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: crate::format::MessageDeltaUsage { output_tokens: 1 },
            },
            create_message_stop(),
        ];

        let mut body = String::new();
        for event in &events {
            body.push_str(&format_sse_event(event));
        }
        sse_ok_response(body, request_id)
    } else {
        let body = crate::format::MessagesResponse {
            id: message_id,
            response_type: "message".to_string(),
            role: crate::format::Role::Assistant,
            content: vec![crate::format::ContentBlock::Text {
                text: "OK".to_string(),
                cache_control: None,
            }],
            model: request.model.clone(),
            stop_reason: Some(crate::format::StopReason::EndTurn),
            stop_sequence: None,
            usage: crate::format::Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        json_ok_response(serde_json::to_vec(&body)?, request_id, Some("BYPASS"))
    };

    response.headers_mut().insert(
        "X-Warmup-Intercepted",
        hyper::header::HeaderValue::from_static("true"),
    );
    Ok(response)
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

async fn read_body_limited(body: hyper::body::Incoming, max_size: usize) -> Result<Bytes, Error> {
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

    Ok(bytes)
}

async fn handle_non_streaming_messages(
    client: &CloudCodeClient,
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
    cache_key: Option<String>,
    state: &Arc<ServerState>,
) -> Result<Response<ResponseBody>, Error> {
    let response = client.send_request(body, access_token, model).await?;
    let anthropic_response = parse_response(&response, model, request_id);
    record_usage(model, &anthropic_response.usage);

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
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let (events, body_bytes) = collect_sse_events(client, body, access_token, model).await?;

    // Log raw response for debugging empty/error responses
    if body_bytes.len() < 2000 {
        let body_str = String::from_utf8_lossy(&body_bytes);
        debug!(
            model = %model,
            request_id = %request_id,
            body_len = body_bytes.len(),
            body = %body_str,
            "Raw SSE response from Google (thinking non-streaming)"
        );
    } else {
        debug!(
            model = %model,
            request_id = %request_id,
            body_len = body_bytes.len(),
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

    if !has_content && !body_bytes.is_empty() {
        let body_str = String::from_utf8_lossy(&body_bytes);
        warn!(
            model = %model,
            request_id = %request_id,
            body_len = body_bytes.len(),
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
    record_usage(model, &anthropic_response.usage);

    log_if_enabled(request_id, "Anthropic response", &anthropic_response);

    let response_body = serde_json::to_vec(&anthropic_response)?;
    Ok(json_ok_response(response_body, request_id, Some("BYPASS")))
}

/// Handle Anthropic streaming messages with true SSE pass-through.
///
/// Returns the response immediately with a channel-backed body.  A background
/// task reads chunks from the upstream Google response, parses them with
/// `SseParser`, and forwards each Anthropic-format SSE event through the
/// channel as it arrives.
async fn handle_streaming_messages(
    client: &CloudCodeClient,
    body: Bytes,
    access_token: &str,
    model: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let upstream = client
        .send_streaming_request(body, access_token, model)
        .await?;

    let (tx, body) = streaming_body();

    let model = model.to_string();
    let request_id_owned = request_id.to_string();

    // Return the SSE response immediately; the background task will feed data.
    let response = sse_streaming_response(body, request_id);

    let request_id = request_id_owned;
    tokio::spawn(async move {
        let mut parser = SseParser::new(&model);
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;
        let mut cache_read_tokens = 0u32;
        let mut has_content = false;
        let mut body_len = 0usize;

        let mut incoming = upstream.into_body();

        // Read chunks from upstream as they arrive.
        loop {
            use http_body_util::BodyExt;
            let frame_timeout = Duration::from_secs(STREAM_FRAME_TIMEOUT_SECS);
            let frame = tokio::time::timeout(frame_timeout, incoming.frame()).await;
            match frame {
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        body_len += data.len();
                        let chunk_str = String::from_utf8_lossy(&data);

                        for event in parser.feed(&chunk_str) {
                            // Track tokens
                            match &event {
                                StreamEvent::MessageStart { message } => {
                                    input_tokens = message.usage.input_tokens;
                                    cache_read_tokens =
                                        message.usage.cache_read_input_tokens.unwrap_or(0);
                                }
                                StreamEvent::MessageDelta { usage, .. } => {
                                    output_tokens = usage.output_tokens;
                                }
                                StreamEvent::ContentBlockStart { .. }
                                | StreamEvent::ContentBlockDelta { .. } => {
                                    has_content = true;
                                }
                                StreamEvent::Error { error } => {
                                    warn!(
                                        model = %model,
                                        request_id = %request_id,
                                        error = %error.message,
                                        "Google API error in SSE stream"
                                    );
                                }
                                _ => {}
                            }

                            let formatted = format_sse_event(&event);
                            if tx.send(Bytes::from(formatted)).await.is_err() {
                                // Client disconnected
                                return;
                            }
                        }
                    }
                }
                Ok(Some(Err(e))) => {
                    warn!(
                        model = %model,
                        request_id = %request_id,
                        error = %e,
                        "Error reading upstream SSE stream"
                    );
                    break;
                }
                Ok(None) => break, // End of upstream stream
                Err(_) => {
                    warn!(
                        model = %model,
                        request_id = %request_id,
                        "Upstream frame timeout in Anthropic streaming"
                    );
                    break;
                }
            }
        }

        // Flush any remaining events from the parser.
        for event in parser.finish() {
            match &event {
                StreamEvent::MessageStart { message } => {
                    input_tokens = message.usage.input_tokens;
                    cache_read_tokens = message.usage.cache_read_input_tokens.unwrap_or(0);
                }
                StreamEvent::MessageDelta { usage, .. } => {
                    output_tokens = usage.output_tokens;
                }
                StreamEvent::ContentBlockStart { .. } | StreamEvent::ContentBlockDelta { .. } => {
                    has_content = true;
                }
                _ => {}
            }
            let formatted = format_sse_event(&event);
            let _ = tx.send(Bytes::from(formatted)).await;
        }

        // Send final message_stop event.
        let stop_event = format_sse_event(&create_message_stop());
        let _ = tx.send(Bytes::from(stop_event)).await;

        // Record token usage.
        get_stats().record_token_usage(&model, input_tokens, output_tokens, cache_read_tokens);

        if !has_content && body_len > 0 {
            warn!(
                model = %model,
                request_id = %request_id,
                body_len = body_len,
                "Empty response from Google API (streaming) - model may be unavailable"
            );
        }
    });

    Ok(response)
}

async fn handle_models() -> Result<Response<ResponseBody>, Error> {
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
        .body(full_body(Full::new(Bytes::from(body))))
        .unwrap())
}

async fn handle_model_by_id(path: &str) -> Result<Response<ResponseBody>, Error> {
    let model_id = if let Some(id) = path.strip_prefix("/v1/models/") {
        id
    } else if let Some(id) = path.strip_prefix("/models/") {
        id
    } else {
        return Ok(openai_error_response(
            StatusCode::NOT_FOUND,
            "Model not found",
            "invalid_request_error",
        ));
    };

    if model_id.is_empty() {
        return Ok(openai_error_response(
            StatusCode::NOT_FOUND,
            "Model not found",
            "invalid_request_error",
        ));
    }

    let canonical = resolve_model_alias(model_id);
    let model = Model::all()
        .iter()
        .find(|m| m.anthropic_id() == canonical)
        .or_else(|| Model::all().iter().find(|m| m.anthropic_id() == model_id));

    let Some(model) = model else {
        return Ok(openai_error_response(
            StatusCode::NOT_FOUND,
            &format!("No model found with id '{}'", model_id),
            "invalid_request_error",
        ));
    };

    let info = ModelInfo {
        id: model.anthropic_id().to_string(),
        model_type: "model".to_string(),
        display_name: model.anthropic_id().to_string(),
        created_at: "2025-01-01T00:00:00Z".to_string(),
    };
    let body = serde_json::to_vec(&info)?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(full_body(Full::new(Bytes::from(body))))
        .unwrap())
}

#[derive(Debug, serde::Deserialize)]
struct ModelDetectRequest {
    #[serde(default)]
    model: Option<String>,
}

fn detect_model_request_type(model: &str) -> &'static str {
    if model.to_ascii_lowercase().contains("image") {
        return "image_gen";
    }

    match get_model_family(model) {
        "claude" => "claude",
        "gemini" => "gemini",
        "gpt-oss" => "gpt-oss",
        _ => "gemini",
    }
}

async fn handle_model_detect(
    req: Request<hyper::body::Incoming>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
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

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let request: ModelDetectRequest = match serde_json::from_slice(&body_bytes) {
        Ok(parsed) => parsed,
        Err(e) => {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            ));
        }
    };

    let Some(model) = request.model else {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Missing required field: model",
            "invalid_request_error",
        ));
    };
    if model.trim().is_empty() {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "model cannot be empty",
            "invalid_request_error",
        ));
    }

    let config = get_config();
    let mapped_model = resolve_with_mappings(
        &model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );
    let request_type = detect_model_request_type(&mapped_model);
    let is_image_gen = request_type == "image_gen";

    let response = serde_json::json!({
        "model": model,
        "mapped_model": mapped_model,
        "type": request_type,
        "features": {
            "has_web_search": false,
            "is_image_gen": is_image_gen
        }
    });

    Ok(json_ok_response(
        serde_json::to_vec(&response)?,
        request_id,
        Some("BYPASS"),
    ))
}

/// Estimate token count for a messages request.
///
/// Uses a chars/4 heuristic which is a reasonable approximation for most
/// tokenizers (GPT, Claude, Gemini all average ~3.5-4.5 chars per token
/// for English text). This avoids requiring a full tokenizer dependency.
async fn handle_count_tokens(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<ResponseBody>, Error> {
    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;

    #[derive(serde::Deserialize)]
    struct CountTokensRequest {
        messages: Vec<crate::format::anthropic::Message>,
        #[serde(default)]
        system: Option<crate::format::anthropic::SystemPrompt>,
        #[serde(default)]
        tools: Option<Vec<crate::format::anthropic::Tool>>,
    }

    let request: CountTokensRequest = serde_json::from_slice(&body_bytes)?;

    let mut total_chars: usize = 0;

    // Count system prompt chars
    if let Some(system) = &request.system {
        match system {
            crate::format::anthropic::SystemPrompt::Text(text) => {
                total_chars += text.len();
            }
            crate::format::anthropic::SystemPrompt::Blocks(blocks) => {
                for block in blocks {
                    total_chars += count_block_chars(block);
                }
            }
        }
    }

    // Count message chars
    for msg in &request.messages {
        match &msg.content {
            crate::format::anthropic::MessageContent::Text(text) => {
                total_chars += text.len();
            }
            crate::format::anthropic::MessageContent::Blocks(blocks) => {
                for block in blocks {
                    total_chars += count_block_chars(block);
                }
            }
        }
    }

    // Count tool definitions
    if let Some(tools) = &request.tools {
        for tool in tools {
            total_chars += tool.name.len();
            if let Some(desc) = &tool.description {
                total_chars += desc.len();
            }
            total_chars += tool.input_schema.to_string().len();
        }
    }

    // Estimate: ~4 chars per token, with a minimum of 1
    let input_tokens = (total_chars / 4).max(1) as u32;

    let response = serde_json::json!({
        "input_tokens": input_tokens,
    });

    let response_body = serde_json::to_vec(&response)?;
    Ok(json_ok_response(response_body, "count_tokens", None))
}

fn parse_gemini_model_path(path: &str, suffix: &str) -> Option<String> {
    let model = path.strip_prefix("/v1beta/models/")?.strip_suffix(suffix)?;
    let model = model.strip_prefix("models/").unwrap_or(model);
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

fn parse_gemini_get_model_path(path: &str) -> Option<String> {
    let model = path.strip_prefix("/v1beta/models/")?;
    if model.is_empty() || model.contains(':') {
        return None;
    }
    let model = model.strip_prefix("models/").unwrap_or(model);
    Some(model.to_string())
}

fn is_known_gemini_model(model: &str) -> bool {
    Model::all()
        .iter()
        .map(Model::anthropic_id)
        .any(|id| id == model && id.starts_with("gemini-"))
}

fn gemini_model_descriptor(model: &str) -> serde_json::Value {
    serde_json::json!({
        "name": format!("models/{model}"),
        "baseModelId": model,
        "version": "agcp",
        "displayName": model,
        "description": "Gemini model via AGCP",
        "inputTokenLimit": 1_048_576,
        "outputTokenLimit": 65_536,
        "supportedGenerationMethods": [
            "generateContent",
            "streamGenerateContent",
            "countTokens"
        ],
        "temperature": 1.0,
        "topP": 1.0,
        "topK": 40
    })
}

async fn handle_gemini_models() -> Result<Response<ResponseBody>, Error> {
    let models: Vec<_> = Model::all()
        .iter()
        .map(Model::anthropic_id)
        .filter(|id| id.starts_with("gemini-"))
        .map(gemini_model_descriptor)
        .collect();

    let response = serde_json::json!({ "models": models });
    Ok(json_ok_response(
        serde_json::to_vec(&response)?,
        "gemini_models",
        None,
    ))
}

async fn handle_gemini_model(path: &str) -> Result<Response<ResponseBody>, Error> {
    let Some(model) = parse_gemini_get_model_path(path) else {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    };

    if !is_known_gemini_model(&model) {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    }

    let response = gemini_model_descriptor(&model);
    Ok(json_ok_response(
        serde_json::to_vec(&response)?,
        "gemini_model",
        None,
    ))
}

fn count_gemini_part_chars(part: &serde_json::Value) -> usize {
    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
        return text.len();
    }

    if part.get("inlineData").is_some() || part.get("inline_data").is_some() {
        return 1024;
    }

    if let Some(function_call) = part
        .get("functionCall")
        .or_else(|| part.get("function_call"))
    {
        return function_call.to_string().len();
    }

    if let Some(function_response) = part
        .get("functionResponse")
        .or_else(|| part.get("function_response"))
    {
        return function_response.to_string().len();
    }

    0
}

fn estimate_gemini_tokens(payload: &serde_json::Value) -> u32 {
    let mut total_chars = 0usize;

    if let Some(system) = payload
        .get("systemInstruction")
        .or_else(|| payload.get("system_instruction"))
        && let Some(parts) = system.get("parts").and_then(|v| v.as_array())
    {
        total_chars += parts.iter().map(count_gemini_part_chars).sum::<usize>();
    }

    if let Some(contents) = payload.get("contents").and_then(|v| v.as_array()) {
        for content in contents {
            if let Some(role) = content.get("role").and_then(|v| v.as_str()) {
                total_chars += role.len();
            }
            if let Some(parts) = content.get("parts").and_then(|v| v.as_array()) {
                total_chars += parts.iter().map(count_gemini_part_chars).sum::<usize>();
            }
        }
    }

    if let Some(tools) = payload.get("tools").and_then(|v| v.as_array()) {
        total_chars += tools.iter().map(|t| t.to_string().len()).sum::<usize>();
    }

    (total_chars / 4).max(1) as u32
}

async fn handle_gemini_count_tokens(
    req: Request<hyper::body::Incoming>,
    path: &str,
) -> Result<Response<ResponseBody>, Error> {
    let Some(model) = parse_gemini_model_path(path, ":countTokens") else {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    };

    if !is_known_gemini_model(&model) {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    }

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let payload: serde_json::Value = serde_json::from_slice(&body_bytes)?;
    let total_tokens = estimate_gemini_tokens(&payload);

    let response = serde_json::json!({
        "totalTokens": total_tokens
    });

    Ok(json_ok_response(
        serde_json::to_vec(&response)?,
        "gemini_count_tokens",
        None,
    ))
}

fn build_gemini_cloudcode_request(
    project_id: &str,
    model: &str,
    mut request: GoogleGenerateContentRequest,
    request_id: &str,
) -> CloudCodeRequest {
    if request.session_id.is_none() {
        request.session_id = Some(request_id.to_string());
    }

    CloudCodeRequest {
        project: project_id.to_string(),
        model: model.to_string(),
        request,
        user_agent: "agcp".to_string(),
        request_type: "api".to_string(),
        request_id: request_id.to_string(),
    }
}

fn record_google_usage(model: &str, usage_metadata: Option<&crate::format::google::UsageMetadata>) {
    if let Some(usage) = usage_metadata {
        let cache_read = usage.cached_content_token_count;
        let input_tokens = usage.prompt_token_count.saturating_sub(cache_read);
        let output_tokens = usage.candidates_token_count;
        get_stats().record_token_usage(model, input_tokens, output_tokens, cache_read);
    }
}

async fn handle_gemini_generate_content(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    path: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    #[cfg(test)]
    let mock_upstream = mock_upstream_enabled(req.headers());
    #[cfg(not(test))]
    let mock_upstream = false;

    let Some(model) = parse_gemini_model_path(path, ":generateContent") else {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    };

    if !is_known_gemini_model(&model) {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    }

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let request: GoogleGenerateContentRequest = serde_json::from_slice(&body_bytes)?;

    get_stats().record_request(&model, "/v1beta/models/:generateContent");

    if mock_upstream {
        let response = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "mock-gemini-response"}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 8,
                "candidatesTokenCount": 3,
                "totalTokenCount": 11,
                "cachedContentTokenCount": 0
            }
        });
        return Ok(json_ok_response(
            serde_json::to_vec(&response)?,
            request_id,
            Some("BYPASS"),
        ));
    }

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(&state, &model).await?;

    let cc_request = build_gemini_cloudcode_request(&project_id, &model, request, request_id);
    let request_body = Bytes::from(serde_json::to_vec(&cc_request)?);

    let result = async {
        let response = state
            .cloudcode_client
            .send_request(request_body, &access_token, &model)
            .await?;
        record_google_usage(&model, response.usage_metadata.as_ref());
        Ok(json_ok_response(
            serde_json::to_vec(&response)?,
            request_id,
            Some("BYPASS"),
        ))
    }
    .await;

    track_request_outcome(
        &state,
        &account_id,
        &account_email,
        &model,
        request_id,
        &result,
    )
    .await;

    result
}

async fn handle_gemini_stream_generate_content(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    path: &str,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    #[cfg(test)]
    let mock_upstream = mock_upstream_enabled(req.headers());
    #[cfg(not(test))]
    let mock_upstream = false;

    let Some(model) = parse_gemini_model_path(path, ":streamGenerateContent") else {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    };

    if !is_known_gemini_model(&model) {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        ));
    }

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let request: GoogleGenerateContentRequest = serde_json::from_slice(&body_bytes)?;

    get_stats().record_request(&model, "/v1beta/models/:streamGenerateContent");

    if mock_upstream {
        let body = "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"mock stream chunk\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":8,\"candidatesTokenCount\":3,\"totalTokenCount\":11,\"cachedContentTokenCount\":0}}}\n\n";
        return Ok(sse_ok_response(body.to_string(), request_id));
    }

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(&state, &model).await?;

    let cc_request = build_gemini_cloudcode_request(&project_id, &model, request, request_id);
    let request_body = Bytes::from(serde_json::to_vec(&cc_request)?);

    let result = async {
        let upstream = state
            .cloudcode_client
            .send_streaming_request(request_body, &access_token, &model)
            .await?;

        let (tx, body) = streaming_body();
        let response = sse_streaming_response(body, request_id);

        tokio::spawn(async move {
            let mut incoming = upstream.into_body();

            loop {
                let frame_timeout = Duration::from_secs(STREAM_FRAME_TIMEOUT_SECS);
                match tokio::time::timeout(frame_timeout, incoming.frame()).await {
                    Ok(Some(Ok(frame))) => {
                        if let Ok(data) = frame.into_data() {
                            let _ = tx.send(data).await;
                        }
                    }
                    Ok(Some(Err(e))) => {
                        warn!(error = %e, "Error reading Gemini upstream stream");
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        warn!("Upstream frame timeout in Gemini streaming");
                        break;
                    }
                }
            }
        });

        Ok(response)
    }
    .await;

    track_request_outcome(
        &state,
        &account_id,
        &account_email,
        &model,
        request_id,
        &result,
    )
    .await;

    result
}

#[derive(Debug, serde::Deserialize)]
struct WarmupRequest {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    refresh_quotas: Option<bool>,
}

async fn handle_internal_warmup(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    let has_enabled_accounts = {
        let accounts = state.accounts.read().await;
        accounts.accounts.iter().any(|a| a.enabled && !a.is_invalid)
    };

    if !has_enabled_accounts {
        return Ok(json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"error","message":"No enabled accounts configured. Run 'agcp login' first."}"#,
        ));
    }

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let warmup_request = if body_bytes.is_empty() {
        WarmupRequest {
            model: None,
            refresh_quotas: None,
        }
    } else {
        serde_json::from_slice::<WarmupRequest>(&body_bytes)?
    };

    let config = get_config();
    let requested_model = warmup_request
        .model
        .unwrap_or_else(|| "gemini-3-flash".to_string());
    let model = resolve_with_mappings(
        &requested_model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );
    let refresh_quotas = warmup_request.refresh_quotas.unwrap_or(true);

    get_stats().record_request(&model, "/internal/warmup");

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(&state, &model).await?;

    let result = async {
        let mut refreshed_models = 0usize;

        // Send a minimal warmup request so first real request has lower cold-start latency.
        let warmup_request = MessagesRequest {
            model: model.clone(),
            messages: vec![crate::format::anthropic::Message {
                role: crate::format::anthropic::Role::User,
                content: crate::format::anthropic::MessageContent::Text(
                    "Warmup ping: reply with OK.".to_string(),
                ),
            }],
            max_tokens: 8,
            stream: false,
            system: None,
            tools: None,
            temperature: Some(0.0),
            top_p: None,
            top_k: None,
            stop_sequences: None,
            tool_choice: None,
            thinking: None,
            response_format: None,
            candidate_count: None,
        };

        let cc_request = build_request(&warmup_request, &project_id);
        let warmup_body = Bytes::from(serde_json::to_vec(&cc_request)?);
        let warmup_response = state
            .cloudcode_client
            .send_request(warmup_body, &access_token, &model)
            .await?;
        record_google_usage(&model, warmup_response.usage_metadata.as_ref());

        if refresh_quotas {
            let quotas = fetch_model_quotas(&state.http_client, &access_token, Some(&project_id))
                .await
                .map_err(Error::Http)?;
            refreshed_models = quotas.len();

            let mut accounts = state.accounts.write().await;
            if let Some(account) = accounts.get_account_mut(&account_id) {
                for quota in &quotas {
                    let reset_time = quota
                        .reset_time
                        .as_ref()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.timestamp() as u64)
                        .unwrap_or(0);

                    account.quota.insert(
                        quota.model_id.clone(),
                        crate::auth::accounts::ModelQuota {
                            remaining_fraction: quota.remaining_fraction,
                            reset_time,
                        },
                    );
                }
            }
            if let Err(e) = accounts.save() {
                warn!(error = %e, "Failed to persist refreshed quotas during warmup");
            }
        }

        let response = serde_json::json!({
            "status": "ok",
            "model": model.clone(),
            "warmed_up": true,
            "refresh_quotas": refresh_quotas,
            "quotas_refreshed": refreshed_models,
        });

        Ok(json_ok_response(
            serde_json::to_vec(&response)?,
            request_id,
            Some("BYPASS"),
        ))
    }
    .await;

    track_request_outcome(
        &state,
        &account_id,
        &account_email,
        &model,
        request_id,
        &result,
    )
    .await;

    result
}

#[derive(Debug, Clone)]
struct MultipartFilePart {
    content_type: Option<String>,
    data: Vec<u8>,
}

#[derive(Debug)]
struct MultipartFormData {
    fields: HashMap<String, Vec<String>>,
    files: HashMap<String, Vec<MultipartFilePart>>,
    file: Option<MultipartFilePart>,
}

fn parse_multipart_boundary(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("boundary="))
        .map(|boundary| boundary.trim_matches('"').to_string())
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|pos| from + pos)
}

fn parse_disposition_param(disposition: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    disposition
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&prefix))
        .map(|value| value.trim_matches('"').to_string())
}

fn parse_multipart_form_data(body: &[u8], boundary: &str) -> Result<MultipartFormData, Error> {
    let marker = format!("--{boundary}");
    let next_marker = format!("\r\n--{boundary}");
    let marker_bytes = marker.as_bytes();
    let next_marker_bytes = next_marker.as_bytes();

    let mut cursor = 0usize;
    let mut form = MultipartFormData {
        fields: HashMap::new(),
        files: HashMap::new(),
        file: None,
    };

    while let Some(start) = find_bytes(body, marker_bytes, cursor) {
        cursor = start + marker_bytes.len();

        // Final boundary marker: --{boundary}--
        if body.get(cursor..cursor + 2) == Some(b"--") {
            break;
        }

        if body.get(cursor..cursor + 2) == Some(b"\r\n") {
            cursor += 2;
        }

        let headers_end = find_bytes(body, b"\r\n\r\n", cursor).ok_or_else(|| {
            Error::Api(ApiError::InvalidRequest {
                message: "Malformed multipart payload".to_string(),
            })
        })?;

        let headers = String::from_utf8_lossy(&body[cursor..headers_end]);
        let data_start = headers_end + 4;

        let data_end = find_bytes(body, next_marker_bytes, data_start).ok_or_else(|| {
            Error::Api(ApiError::InvalidRequest {
                message: "Malformed multipart payload".to_string(),
            })
        })?;

        let part_data = &body[data_start..data_end];
        cursor = data_end + 2;

        let mut field_name: Option<String> = None;
        let mut filename: Option<String> = None;
        let mut part_content_type: Option<String> = None;

        for line in headers.lines() {
            let line = line.trim();
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                field_name = parse_disposition_param(line, "name");
                filename = parse_disposition_param(line, "filename");
            } else if lower.starts_with("content-type:")
                && let Some((_, value)) = line.split_once(':')
            {
                part_content_type = Some(value.trim().to_string());
            }
        }

        if let Some(name) = field_name {
            if name == "file" || filename.is_some() {
                let file_part = MultipartFilePart {
                    content_type: part_content_type,
                    data: part_data.to_vec(),
                };
                form.files.entry(name).or_default().push(file_part.clone());
                if form.file.is_none() {
                    form.file = Some(file_part);
                }
            } else {
                let value = String::from_utf8_lossy(part_data).trim().to_string();
                form.fields.entry(name).or_default().push(value);
            }
        }
    }

    Ok(form)
}

fn multipart_first_field<'a>(
    fields: &'a HashMap<String, Vec<String>>,
    key: &str,
) -> Option<&'a str> {
    fields.get(key)?.first().map(|v| v.as_str())
}

fn multipart_field_values(fields: &HashMap<String, Vec<String>>, key: &str) -> Vec<String> {
    fields.get(key).cloned().unwrap_or_default()
}

fn multipart_first_file<'a>(
    multipart: &'a MultipartFormData,
    key: &str,
) -> Option<&'a MultipartFilePart> {
    multipart.files.get(key).and_then(|values| values.first())
}

fn extract_text_content(content: &[crate::format::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            crate::format::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn subtitle_timestamp(seconds: f64, webvtt: bool) -> String {
    let total_ms = (seconds * 1000.0).round().max(0.0) as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    if webvtt {
        format!("{h:02}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{h:02}:{m:02}:{s:02},{ms:03}")
    }
}

fn to_srt(transcript: &str) -> String {
    let start = subtitle_timestamp(0.0, false);
    let end = subtitle_timestamp(30.0, false);
    format!("1\n{start} --> {end}\n{}\n", transcript.trim())
}

fn to_vtt(transcript: &str) -> String {
    let start = subtitle_timestamp(0.0, true);
    let end = subtitle_timestamp(30.0, true);
    format!("WEBVTT\n\n{start} --> {end}\n{}\n", transcript.trim())
}

fn build_transcription_response(
    response_format: &str,
    transcript: &str,
    language: &str,
    include_word_timestamps: bool,
    include_segment_timestamps: bool,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    match response_format {
        "text" => Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(full_body(Full::new(Bytes::from(transcript.to_string()))))
            .unwrap()),
        "srt" => Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(full_body(Full::new(Bytes::from(to_srt(transcript)))))
            .unwrap()),
        "vtt" => Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/vtt; charset=utf-8")
            .body(full_body(Full::new(Bytes::from(to_vtt(transcript)))))
            .unwrap()),
        "verbose_json" => {
            let mut response = serde_json::json!({
                "task": "transcribe",
                "language": language,
                "duration": serde_json::Value::Null,
                "text": transcript,
            });

            if include_word_timestamps {
                response["words"] = serde_json::Value::Array(Vec::new());
            }
            if include_segment_timestamps {
                response["segments"] = serde_json::json!([{
                    "id": 0,
                    "seek": 0,
                    "start": 0.0,
                    "end": 30.0,
                    "text": transcript,
                    "tokens": [],
                    "temperature": 0.0,
                    "avg_logprob": 0.0,
                    "compression_ratio": 0.0,
                    "no_speech_prob": 0.0
                }]);
            }

            Ok(json_ok_response(
                serde_json::to_vec(&response)?,
                request_id,
                Some("BYPASS"),
            ))
        }
        _ => {
            let response = serde_json::json!({ "text": transcript });
            Ok(json_ok_response(
                serde_json::to_vec(&response)?,
                request_id,
                Some("BYPASS"),
            ))
        }
    }
}

fn parse_timestamp_granularities(fields: &HashMap<String, Vec<String>>) -> (bool, bool) {
    let mut values = multipart_field_values(fields, "timestamp_granularities[]");
    values.extend(multipart_field_values(fields, "timestamp_granularities"));
    let has_word = values.iter().any(|v| v.eq_ignore_ascii_case("word"));
    let has_segment = values.iter().any(|v| v.eq_ignore_ascii_case("segment"));

    // Default behavior is segment-level timestamps when not specified.
    if values.is_empty() {
        (false, true)
    } else {
        (has_word, has_segment)
    }
}

#[cfg(test)]
fn mock_upstream_enabled(headers: &hyper::HeaderMap) -> bool {
    headers
        .get("x-agcp-mock-upstream")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

async fn handle_audio_transcriptions(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    #[cfg(test)]
    let mock_upstream = mock_upstream_enabled(req.headers());
    #[cfg(not(test))]
    let mock_upstream = false;

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_type.starts_with("multipart/form-data") {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Content-Type must be multipart/form-data",
            "invalid_request_error",
        ));
    }

    let Some(boundary) = parse_multipart_boundary(content_type) else {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Missing multipart boundary",
            "invalid_request_error",
        ));
    };

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let multipart = parse_multipart_form_data(&body_bytes, &boundary)?;
    let Some(file) = multipart.file else {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Missing required multipart field 'file'",
            "invalid_request_error",
        ));
    };

    if file.data.is_empty() {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Uploaded audio file is empty",
            "invalid_request_error",
        ));
    }

    let response_format = multipart
        .fields
        .get("response_format")
        .and_then(|values| values.first())
        .map(|s| s.as_str())
        .unwrap_or("json");
    if !matches!(
        response_format,
        "json" | "text" | "verbose_json" | "srt" | "vtt"
    ) {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Unsupported response_format (supported: json, text, verbose_json, srt, vtt)",
            "invalid_request_error",
        ));
    }

    let (include_word_timestamps, include_segment_timestamps) =
        parse_timestamp_granularities(&multipart.fields);
    let language = multipart_first_field(&multipart.fields, "language")
        .filter(|lang| !lang.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();

    if mock_upstream {
        return build_transcription_response(
            response_format,
            "mock transcription from agcp test upstream",
            &language,
            include_word_timestamps,
            include_segment_timestamps,
            request_id,
        );
    }

    let requested_model = multipart
        .fields
        .get("model")
        .and_then(|values| values.first().cloned())
        .unwrap_or_else(|| "gemini-3-flash".to_string());
    let config = get_config();
    let model = resolve_with_mappings(
        &requested_model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );

    let mut instruction =
        "Transcribe the provided audio verbatim. Return only the transcription.".to_string();
    if let Some(language_hint) = multipart_first_field(&multipart.fields, "language")
        && !language_hint.trim().is_empty()
    {
        instruction.push_str(&format!(" The expected language is '{language_hint}'."));
    }
    if let Some(prompt) = multipart_first_field(&multipart.fields, "prompt")
        && !prompt.trim().is_empty()
    {
        instruction.push_str(&format!(" Additional context: {prompt}"));
    }

    let media_type = file.content_type.unwrap_or_else(|| "audio/wav".to_string());
    let audio_b64 = base64::engine::general_purpose::STANDARD.encode(file.data);

    let messages_request = MessagesRequest {
        model: model.clone(),
        messages: vec![crate::format::anthropic::Message {
            role: crate::format::anthropic::Role::User,
            content: crate::format::anthropic::MessageContent::Blocks(vec![
                crate::format::anthropic::ContentBlock::Document {
                    source: crate::format::anthropic::DocumentSource {
                        source_type: "base64".to_string(),
                        media_type,
                        data: audio_b64,
                    },
                    cache_control: None,
                },
                crate::format::anthropic::ContentBlock::Text {
                    text: instruction,
                    cache_control: None,
                },
            ]),
        }],
        max_tokens: 4096,
        stream: false,
        system: None,
        tools: None,
        temperature: Some(0.0),
        top_p: None,
        top_k: None,
        stop_sequences: None,
        tool_choice: None,
        thinking: None,
        response_format: None,
        candidate_count: None,
    };

    get_stats().record_request(&model, "/v1/audio/transcriptions");

    let response =
        execute_messages_request(&messages_request, &state, request_id, false, true).await?;
    let response_bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|_| Error::Http("Failed reading transcription response body".to_string()))?
        .to_bytes();

    let anthropic_response: crate::format::anthropic::MessagesResponse =
        serde_json::from_slice(&response_bytes)?;
    let transcript = extract_text_content(&anthropic_response.content);

    if transcript.trim().is_empty() {
        return Ok(openai_error_response(
            StatusCode::BAD_GATEWAY,
            "Transcription model returned no text",
            "api_error",
        ));
    }

    build_transcription_response(
        response_format,
        &transcript,
        &language,
        include_word_timestamps,
        include_segment_timestamps,
        request_id,
    )
}

#[derive(Debug, serde::Deserialize)]
struct ImagesGenerationsRequest {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    response_format: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

fn extract_image_items_from_google_response(
    response: crate::format::google::GenerateContentResponse,
    response_format: &str,
) -> Result<(Vec<serde_json::Value>, Option<String>), String> {
    let mut data_items = Vec::new();
    let mut revised_prompt: Option<String> = None;

    if let Some(candidates) = response.candidates {
        for candidate in candidates {
            if let Some(content) = candidate.content {
                for part in content.parts {
                    match part {
                        GooglePart::InlineData(inline) => {
                            let mime_type = inline.inline_data.mime_type;
                            let b64_data = inline.inline_data.data;
                            if response_format == "url" {
                                data_items.push(serde_json::json!({
                                    "url": format!("data:{mime_type};base64,{b64_data}")
                                }));
                            } else {
                                data_items.push(serde_json::json!({
                                    "b64_json": b64_data
                                }));
                            }
                        }
                        GooglePart::Text(text) => {
                            if revised_prompt.is_none() && !text.text.trim().is_empty() {
                                revised_prompt = Some(text.text.trim().to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if data_items.is_empty() {
        let message = if revised_prompt.is_some() {
            "Upstream model returned text instead of image data"
        } else {
            "Upstream model did not return any image data"
        };
        return Err(message.to_string());
    }

    if let Some(revised) = &revised_prompt {
        for item in &mut data_items {
            if let Some(obj) = item.as_object_mut() {
                obj.insert(
                    "revised_prompt".to_string(),
                    serde_json::Value::String(revised.clone()),
                );
            }
        }
    }

    Ok((data_items, revised_prompt))
}

#[derive(Debug, Clone, Copy)]
enum ImageEditMode {
    Edits,
    Variations,
}

impl ImageEditMode {
    fn route(self) -> &'static str {
        match self {
            ImageEditMode::Edits => "/v1/images/edits",
            ImageEditMode::Variations => "/v1/images/variations",
        }
    }

    fn action(self) -> &'static str {
        match self {
            ImageEditMode::Edits => "Edit the provided image according to the prompt.",
            ImageEditMode::Variations => "Create variations of the provided image.",
        }
    }

    fn requires_prompt(self) -> bool {
        matches!(self, ImageEditMode::Edits)
    }
}

fn validate_openai_image_params(
    response_format: &str,
    candidate_count: u32,
    size: Option<&str>,
    quality: Option<&str>,
    style: Option<&str>,
) -> Option<Response<ResponseBody>> {
    if !matches!(response_format, "b64_json" | "url") {
        return Some(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Unsupported response_format (supported: b64_json, url)",
            "invalid_request_error",
        ));
    }

    if !(1..=10).contains(&candidate_count) {
        return Some(openai_error_response(
            StatusCode::BAD_REQUEST,
            "n must be between 1 and 10",
            "invalid_request_error",
        ));
    }

    if let Some(size) = size {
        let valid_sizes = [
            "256x256",
            "512x512",
            "1024x1024",
            "1024x1536",
            "1536x1024",
            "auto",
        ];
        if !valid_sizes.iter().any(|s| s == &size) {
            return Some(openai_error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported size (supported: 256x256, 512x512, 1024x1024, 1024x1536, 1536x1024, auto)",
                "invalid_request_error",
            ));
        }
    }

    if let Some(quality) = quality {
        let valid_qualities = ["low", "medium", "high", "standard", "hd", "auto"];
        if !valid_qualities.iter().any(|q| q == &quality) {
            return Some(openai_error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported quality (supported: low, medium, high, standard, hd, auto)",
                "invalid_request_error",
            ));
        }
    }

    if let Some(style) = style {
        let valid_styles = ["vivid", "natural"];
        if !valid_styles.iter().any(|s| s == &style) {
            return Some(openai_error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported style (supported: vivid, natural)",
                "invalid_request_error",
            ));
        }
    }

    None
}

fn build_mock_image_response(
    response_format: &str,
    candidate_count: u32,
    revised_prompt_seed: &str,
) -> serde_json::Value {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let revised_prompt = format!("mock revised prompt for: {}", revised_prompt_seed.trim());
    let mut items = Vec::new();
    for i in 0..candidate_count {
        let item = if response_format == "url" {
            serde_json::json!({
                "url": format!("data:image/png;base64,MOCK_IMAGE_DATA_{}", i + 1),
                "revised_prompt": revised_prompt,
            })
        } else {
            serde_json::json!({
                "b64_json": format!("MOCK_IMAGE_DATA_{}", i + 1),
                "revised_prompt": revised_prompt,
            })
        };
        items.push(item);
    }
    serde_json::json!({
        "created": created,
        "data": items
    })
}

fn image_file_to_google_part(file: &MultipartFilePart, default_mime: &str) -> GooglePart {
    let mime_type = file
        .content_type
        .as_deref()
        .filter(|m| m.starts_with("image/"))
        .unwrap_or(default_mime)
        .to_string();
    let data = base64::engine::general_purpose::STANDARD.encode(&file.data);
    GooglePart::InlineData(crate::format::google::InlineDataPart {
        inline_data: crate::format::google::InlineData { mime_type, data },
    })
}

fn collect_candidate_image_files(multipart: &MultipartFormData) -> Vec<&MultipartFilePart> {
    let mut keys: Vec<&str> = multipart.files.keys().map(String::as_str).collect();
    keys.sort_unstable();

    let mut files = Vec::new();
    for key in keys {
        if (key == "image" || key == "file" || key.starts_with("image"))
            && let Some(values) = multipart.files.get(key)
        {
            files.extend(values.iter());
        }
    }
    files
}

fn select_primary_image(multipart: &MultipartFormData) -> Option<&MultipartFilePart> {
    multipart_first_file(multipart, "image")
        .or_else(|| multipart_first_file(multipart, "file"))
        .or_else(|| {
            let mut keys: Vec<&str> = multipart.files.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for key in keys {
                if key.starts_with("image")
                    && let Some(file) = multipart_first_file(multipart, key)
                {
                    return Some(file);
                }
            }
            None
        })
}

async fn handle_images_edit_like(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
    mode: ImageEditMode,
) -> Result<Response<ResponseBody>, Error> {
    #[cfg(test)]
    let mock_upstream = mock_upstream_enabled(req.headers());
    #[cfg(not(test))]
    let mock_upstream = false;

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_type.starts_with("multipart/form-data") {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Content-Type must be multipart/form-data",
            "invalid_request_error",
        ));
    }

    let Some(boundary) = parse_multipart_boundary(content_type) else {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Missing multipart boundary",
            "invalid_request_error",
        ));
    };

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let multipart = parse_multipart_form_data(&body_bytes, &boundary)?;

    let Some(primary_image) = select_primary_image(&multipart) else {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Missing required multipart field 'image'",
            "invalid_request_error",
        ));
    };
    if primary_image.data.is_empty() {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Uploaded image is empty",
            "invalid_request_error",
        ));
    }

    let prompt = multipart_first_field(&multipart.fields, "prompt")
        .unwrap_or_default()
        .trim()
        .to_string();
    if mode.requires_prompt() && prompt.is_empty() {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Missing required multipart field 'prompt'",
            "invalid_request_error",
        ));
    }

    let response_format = multipart_first_field(&multipart.fields, "response_format")
        .unwrap_or("b64_json")
        .to_string();
    let candidate_count = multipart_first_field(&multipart.fields, "n")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(1);
    let size = multipart_first_field(&multipart.fields, "size");
    let quality = multipart_first_field(&multipart.fields, "quality");
    let style = multipart_first_field(&multipart.fields, "style");
    let user = multipart_first_field(&multipart.fields, "user");
    let mask = multipart_first_file(&multipart, "mask");

    if let Some(validation_error) =
        validate_openai_image_params(&response_format, candidate_count, size, quality, style)
    {
        return Ok(validation_error);
    }

    let requested_model = multipart_first_field(&multipart.fields, "model")
        .unwrap_or("gemini-3-flash")
        .to_string();
    let config = get_config();
    let model = resolve_with_mappings(
        &requested_model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );
    get_stats().record_request(&model, mode.route());

    let prompt_seed = if prompt.is_empty() {
        mode.action().to_string()
    } else {
        prompt.clone()
    };
    if mock_upstream {
        let body = build_mock_image_response(&response_format, candidate_count, &prompt_seed);
        return Ok(json_ok_response(
            serde_json::to_vec(&body)?,
            request_id,
            Some("BYPASS"),
        ));
    }

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(&state, &model).await?;

    let mut instruction = format!("{} Return image data only.", mode.action());
    if !prompt.is_empty() {
        instruction.push_str(&format!("\n\nPrompt: {prompt}"));
    }
    if let Some(size) = size {
        instruction.push_str(&format!(" Preferred size: {size}."));
    }
    if let Some(quality) = quality {
        instruction.push_str(&format!(" Preferred quality: {quality}."));
    }
    if let Some(style) = style {
        instruction.push_str(&format!(" Preferred style: {style}."));
    }
    if let Some(user) = user {
        instruction.push_str(&format!(" User context: {user}."));
    }
    if mask.is_some() && matches!(mode, ImageEditMode::Edits) {
        instruction.push_str(" Use the provided mask to focus edits.");
    }

    let mut parts = vec![GooglePart::Text(GoogleTextPart { text: instruction })];
    parts.push(image_file_to_google_part(primary_image, "image/png"));

    if matches!(mode, ImageEditMode::Edits)
        && let Some(mask_file) = mask
    {
        parts.push(image_file_to_google_part(mask_file, "image/png"));
    }

    let candidate_files = collect_candidate_image_files(&multipart);
    let mut consumed_primary = false;
    for file in candidate_files {
        if !consumed_primary && std::ptr::eq(file, primary_image) {
            consumed_primary = true;
            continue;
        }
        parts.push(image_file_to_google_part(file, "image/png"));
    }

    let google_request = GoogleGenerateContentRequest {
        contents: vec![GoogleContent {
            role: "user".to_string(),
            parts,
        }],
        system_instruction: None,
        generation_config: Some(GoogleGenerationConfig {
            response_mime_type: Some("image/png".to_string()),
            candidate_count: Some(candidate_count),
            ..GoogleGenerationConfig::default()
        }),
        tools: None,
        tool_config: None,
        session_id: None,
    };

    let cc_request =
        build_gemini_cloudcode_request(&project_id, &model, google_request, request_id);
    let request_body = Bytes::from(serde_json::to_vec(&cc_request)?);

    let result = async {
        let response = state
            .cloudcode_client
            .send_request(request_body, &access_token, &model)
            .await?;

        record_google_usage(&model, response.usage_metadata.as_ref());

        let (data_items, _revised_prompt) =
            match extract_image_items_from_google_response(response, &response_format) {
                Ok(values) => values,
                Err(message) => {
                    return Ok(openai_error_response(
                        StatusCode::BAD_GATEWAY,
                        &message,
                        "api_error",
                    ));
                }
            };

        if data_items.is_empty() {
            return Ok(openai_error_response(
                StatusCode::BAD_GATEWAY,
                "Upstream model did not return any image data",
                "api_error",
            ));
        }

        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let image_response = serde_json::json!({
            "created": created,
            "data": data_items
        });

        Ok(json_ok_response(
            serde_json::to_vec(&image_response)?,
            request_id,
            Some("BYPASS"),
        ))
    }
    .await;

    track_request_outcome(
        &state,
        &account_id,
        &account_email,
        &model,
        request_id,
        &result,
    )
    .await;

    result
}

async fn handle_images_generations(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
    request_id: &str,
) -> Result<Response<ResponseBody>, Error> {
    #[cfg(test)]
    let mock_upstream = mock_upstream_enabled(req.headers());
    #[cfg(not(test))]
    let mock_upstream = false;

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

    let body_bytes = read_body_limited(req.into_body(), max_request_size()).await?;
    let request: ImagesGenerationsRequest = match serde_json::from_slice(&body_bytes) {
        Ok(parsed) => parsed,
        Err(e) => {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            ));
        }
    };

    let Some(prompt) = request.prompt else {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Missing required field: prompt",
            "invalid_request_error",
        ));
    };

    if prompt.trim().is_empty() {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "prompt cannot be empty",
            "invalid_request_error",
        ));
    }

    let response_format = request
        .response_format
        .unwrap_or_else(|| "b64_json".to_string());
    if !matches!(response_format.as_str(), "b64_json" | "url") {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "Unsupported response_format (supported: b64_json, url)",
            "invalid_request_error",
        ));
    }

    let candidate_count = request.n.unwrap_or(1);
    if !(1..=10).contains(&candidate_count) {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "n must be between 1 and 10",
            "invalid_request_error",
        ));
    }

    if let Some(size) = &request.size {
        let valid_sizes = [
            "256x256",
            "512x512",
            "1024x1024",
            "1024x1536",
            "1536x1024",
            "auto",
        ];
        if !valid_sizes.iter().any(|s| s == size) {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported size (supported: 256x256, 512x512, 1024x1024, 1024x1536, 1536x1024, auto)",
                "invalid_request_error",
            ));
        }
    }

    if let Some(quality) = &request.quality {
        let valid_qualities = ["low", "medium", "high", "standard", "hd", "auto"];
        if !valid_qualities.iter().any(|q| q == quality) {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported quality (supported: low, medium, high, standard, hd, auto)",
                "invalid_request_error",
            ));
        }
    }

    if let Some(style) = &request.style {
        let valid_styles = ["vivid", "natural"];
        if !valid_styles.iter().any(|s| s == style) {
            return Ok(openai_error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported style (supported: vivid, natural)",
                "invalid_request_error",
            ));
        }
    }

    let requested_model = request
        .model
        .unwrap_or_else(|| "gemini-3-flash".to_string());
    let config = get_config();
    let model = resolve_with_mappings(
        &requested_model,
        &config.mappings.rules,
        &config.mappings.background_task_model,
    );

    get_stats().record_request(&model, "/v1/images/generations");

    if mock_upstream {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let revised_prompt = format!("mock revised prompt for: {}", prompt.trim());
        let mut items = Vec::new();
        for i in 0..candidate_count {
            let item = if response_format == "url" {
                serde_json::json!({
                    "url": format!("data:image/png;base64,MOCK_IMAGE_DATA_{}", i + 1),
                    "revised_prompt": revised_prompt,
                })
            } else {
                serde_json::json!({
                    "b64_json": format!("MOCK_IMAGE_DATA_{}", i + 1),
                    "revised_prompt": revised_prompt,
                })
            };
            items.push(item);
        }
        let body = serde_json::json!({
            "created": created,
            "data": items
        });
        return Ok(json_ok_response(
            serde_json::to_vec(&body)?,
            request_id,
            Some("BYPASS"),
        ));
    }

    let (access_token, project_id, account_id, account_email) =
        get_account_credentials(&state, &model).await?;

    let mut prompt_text =
        "Generate an image for the following prompt. Return image data only.".to_string();
    if let Some(size) = request.size {
        prompt_text.push_str(&format!(" Preferred size: {size}."));
    }
    if let Some(quality) = request.quality {
        prompt_text.push_str(&format!(" Preferred quality: {quality}."));
    }
    if let Some(style) = request.style {
        prompt_text.push_str(&format!(" Preferred style: {style}."));
    }
    if let Some(user) = request.user {
        prompt_text.push_str(&format!(" User context: {user}."));
    }
    prompt_text.push_str(&format!("\n\nPrompt: {prompt}"));

    let google_request = GoogleGenerateContentRequest {
        contents: vec![GoogleContent {
            role: "user".to_string(),
            parts: vec![GooglePart::Text(GoogleTextPart { text: prompt_text })],
        }],
        system_instruction: None,
        generation_config: Some(GoogleGenerationConfig {
            response_mime_type: Some("image/png".to_string()),
            candidate_count: Some(candidate_count),
            ..GoogleGenerationConfig::default()
        }),
        tools: None,
        tool_config: None,
        session_id: None,
    };

    let cc_request =
        build_gemini_cloudcode_request(&project_id, &model, google_request, request_id);
    let request_body = Bytes::from(serde_json::to_vec(&cc_request)?);

    let result = async {
        let response = state
            .cloudcode_client
            .send_request(request_body, &access_token, &model)
            .await?;

        record_google_usage(&model, response.usage_metadata.as_ref());

        let (data_items, _revised_prompt) =
            match extract_image_items_from_google_response(response, &response_format) {
                Ok(values) => values,
                Err(message) => {
                    return Ok(openai_error_response(
                        StatusCode::BAD_GATEWAY,
                        &message,
                        "api_error",
                    ));
                }
            };

        if data_items.is_empty() {
            return Ok(openai_error_response(
                StatusCode::BAD_GATEWAY,
                "Upstream model did not return any image data",
                "api_error",
            ));
        }

        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let image_response = serde_json::json!({
            "created": created,
            "data": data_items
        });

        Ok(json_ok_response(
            serde_json::to_vec(&image_response)?,
            request_id,
            Some("BYPASS"),
        ))
    }
    .await;

    track_request_outcome(
        &state,
        &account_id,
        &account_email,
        &model,
        request_id,
        &result,
    )
    .await;

    result
}

/// Count approximate character length of a content block.
fn count_block_chars(block: &crate::format::ContentBlock) -> usize {
    match block {
        crate::format::ContentBlock::Text { text, .. } => text.len(),
        crate::format::ContentBlock::Image { .. } => 256, // Images counted as ~64 tokens
        crate::format::ContentBlock::Document { .. } => 1024, // PDFs counted as ~256 tokens
        crate::format::ContentBlock::ToolUse { name, input, .. } => {
            name.len() + input.to_string().len()
        }
        crate::format::ContentBlock::ToolResult { content, .. } => match content {
            crate::format::anthropic::ToolResultContent::Text(text) => text.len(),
            crate::format::anthropic::ToolResultContent::Blocks(blocks) => {
                blocks.iter().map(count_block_chars).sum()
            }
        },
        crate::format::ContentBlock::Thinking { thinking, .. } => thinking.len(),
    }
}

async fn handle_stats(state: &Arc<ServerState>) -> Result<Response<ResponseBody>, Error> {
    let stats = get_stats().summary();
    let cache_stats = state.cache.lock().await.stats();

    let response = serde_json::json!({
        "requests": stats.to_json(),
        "cache": cache_stats,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(full_body(Full::new(Bytes::from(response.to_string()))))
        .unwrap())
}

async fn handle_account_limits(state: &Arc<ServerState>) -> Result<Response<ResponseBody>, Error> {
    // Get credentials using the existing pattern
    let credentials = get_account_credentials(state, "claude-sonnet-4-6").await;

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
        .body(full_body(Full::new(Bytes::from(response.to_string()))))
        .unwrap())
}

async fn handle_logs_stream() -> Result<Response<ResponseBody>, Error> {
    let log_path = logs_file_path();
    let initial_lines = read_log_tail_lines(&log_path, LOG_STREAM_TAIL_LINES);
    let (tx, body) = streaming_body();

    tokio::spawn(async move {
        // Send recent history first so clients render immediately.
        for line in initial_lines {
            let clean = strip_ansi_codes(&line);
            if tx
                .send(Bytes::from(format!("data: {clean}\n\n")))
                .await
                .is_err()
            {
                return;
            }
        }

        let mut file_offset = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
        let mut trailing_partial = String::new();

        let mut poll_interval =
            tokio::time::interval(Duration::from_millis(LOG_STREAM_POLL_INTERVAL_MS));
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut heartbeat_interval =
            tokio::time::interval(Duration::from_secs(LOG_STREAM_HEARTBEAT_SECS));
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate tick so heartbeats follow the configured cadence.
        heartbeat_interval.tick().await;

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    let current_len = match std::fs::metadata(&log_path) {
                        Ok(meta) => meta.len(),
                        Err(_) => 0,
                    };

                    if current_len < file_offset {
                        // Log file was rotated/truncated.
                        file_offset = 0;
                        trailing_partial.clear();
                    }

                    if current_len == file_offset {
                        continue;
                    }

                    use std::io::{Read, Seek, SeekFrom};
                    let mut file = match std::fs::File::open(&log_path) {
                        Ok(file) => file,
                        Err(_) => continue,
                    };

                    if file.seek(SeekFrom::Start(file_offset)).is_err() {
                        continue;
                    }

                    let mut buf = Vec::new();
                    if file.read_to_end(&mut buf).is_err() {
                        continue;
                    }

                    file_offset = current_len;
                    trailing_partial.push_str(&String::from_utf8_lossy(&buf));

                    let mut chunks: Vec<&str> = trailing_partial.split('\n').collect();
                    let mut next_partial = String::new();
                    if !trailing_partial.ends_with('\n')
                        && let Some(last) = chunks.pop()
                    {
                        next_partial = last.to_string();
                    }

                    for chunk in chunks {
                        let line = strip_ansi_codes(chunk.trim_end_matches('\r'));
                        if tx
                            .send(Bytes::from(format!("data: {line}\n\n")))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }

                    trailing_partial = next_partial;
                }
                _ = heartbeat_interval.tick() => {
                    if tx.send(Bytes::from(": ping\n\n")).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap())
}

fn logs_file_path() -> PathBuf {
    let config_dir = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config").join("agcp"))
        .unwrap_or_else(|| PathBuf::from("."));
    config_dir.join("agcp.log")
}

fn read_log_tail_lines(log_path: &Path, tail_count: usize) -> Vec<String> {
    match std::fs::File::open(log_path) {
        Ok(mut file) => {
            use std::io::{Read, Seek, SeekFrom};
            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len == 0 {
                vec!["No log entries".to_string()]
            } else {
                const CHUNK_SIZE: u64 = 64 * 1024;
                let mut collected: Vec<String> = Vec::new();
                let mut remaining = file_len;

                while remaining > 0 && collected.len() < tail_count + 1 {
                    let chunk = remaining.min(CHUNK_SIZE);
                    let offset = remaining - chunk;
                    let _ = file.seek(SeekFrom::Start(offset));
                    let mut buf = vec![0u8; chunk as usize];
                    if file.read_exact(&mut buf).is_err() {
                        break;
                    }
                    let chunk_str = String::from_utf8_lossy(&buf);
                    let mut chunk_lines: Vec<String> =
                        chunk_str.lines().map(String::from).collect();
                    if offset > 0 && !chunk_lines.is_empty() {
                        let partial = chunk_lines.remove(0);
                        if let Some(last) = collected.last_mut() {
                            *last = format!("{}{}", partial, last);
                        }
                    }
                    chunk_lines.append(&mut collected);
                    collected = chunk_lines;
                    remaining = offset;
                }

                let start = collected.len().saturating_sub(tail_count);
                collected[start..].to_vec()
            }
        }
        Err(_) => vec!["No log file available".to_string()],
    }
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
///
/// Returns `(events, body_bytes)` where `body_bytes` are the raw response bytes.
/// Callers that need the body as a string for logging can convert lazily.
async fn collect_sse_events(
    client: &CloudCodeClient,
    body: Bytes,
    access_token: &str,
    model: &str,
) -> Result<(Vec<StreamEvent>, Bytes), Error> {
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

    // Parse directly from the byte slice (lossy), avoiding an owned String allocation
    let body_str = String::from_utf8_lossy(&body_bytes);

    let mut events = Vec::new();
    for event in parser.feed(&body_str) {
        events.push(event);
    }
    for event in parser.finish() {
        events.push(event);
    }

    Ok((events, body_bytes))
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

fn json_response(status: StatusCode, body: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        .header(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, X-API-Key, X-No-Cache, Cache-Control",
        )
        .body(full_body(Full::new(Bytes::from(body.to_string()))))
        .unwrap()
}

/// CORS preflight response.
fn cors_preflight_response() -> Response<ResponseBody> {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        .header(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, X-API-Key, X-No-Cache, Cache-Control",
        )
        .header("Access-Control-Max-Age", "86400")
        .body(full_body(Full::new(Bytes::new())))
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
fn json_ok_response(
    body: impl Into<Bytes>,
    request_id: &str,
    cache: Option<&str>,
) -> Response<ResponseBody> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("X-Request-Id", request_id)
        .header("Access-Control-Allow-Origin", "*");

    if let Some(cache_status) = cache {
        builder = builder.header("X-Cache", cache_status);
    }

    builder.body(full_body(Full::new(body.into()))).unwrap()
}

/// Build a true SSE streaming response backed by a channel body.
fn sse_streaming_response(body: ResponseBody, request_id: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .header("X-Request-Id", request_id)
        .header("X-Cache", "BYPASS")
        .header("Access-Control-Allow-Origin", "*")
        .body(body)
        .unwrap()
}

/// Build a buffered SSE response with standard headers (used for non-true-streaming paths).
#[allow(dead_code)]
fn sse_ok_response(body: String, request_id: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .header("X-Request-Id", request_id)
        .header("X-Cache", "BYPASS")
        .body(full_body(Full::new(Bytes::from(body))))
        .unwrap()
}

fn map_error(error: &Error) -> (StatusCode, &'static str, String) {
    match error {
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
    }
}

fn error_to_response(error: &Error, request_id: &str, path: &str) -> Response<ResponseBody> {
    let (status, error_type, message) = map_error(error);

    // Add suggestion if available
    let message_with_suggestion = if let Some(suggestion) = error.suggestion() {
        format!("{}. {}", message, suggestion)
    } else {
        message
    };

    error_response_for_path(
        status,
        &message_with_suggestion,
        error_type,
        path,
        request_id,
    )
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

    fn build_multipart_body(
        boundary: &str,
        fields: &[(&str, &str)],
        file_name: &str,
        file_content_type: &str,
        file_data: &str,
    ) -> String {
        build_multipart_body_with_files(
            boundary,
            fields,
            &[("file", file_name, file_content_type, file_data)],
        )
    }

    fn build_multipart_body_with_files(
        boundary: &str,
        fields: &[(&str, &str)],
        files: &[(&str, &str, &str, &str)],
    ) -> String {
        let mut body = String::new();
        for (name, value) in fields {
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ));
        }
        for (field_name, file_name, file_content_type, file_data) in files {
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field_name}\"; filename=\"{file_name}\"\r\nContent-Type: {file_content_type}\r\n\r\n{file_data}\r\n"
            ));
        }
        body.push_str(&format!("--{boundary}--\r\n"));
        body
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

    #[tokio::test]
    async fn test_healthz_check() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
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
            body.contains("gemini-3-flash"),
            "body should list Gemini models: {body}"
        );
    }

    #[tokio::test]
    async fn test_models_endpoint_no_v1_alias() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(
            body.contains("claude-opus-4-6-thinking"),
            "body should list Claude models: {body}"
        );
        assert!(
            body.contains("gemini-3-flash"),
            "body should list Gemini models: {body}"
        );
    }

    #[tokio::test]
    async fn test_model_by_id_endpoint() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /v1/models/gemini-3-flash HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");

        let json: serde_json::Value =
            serde_json::from_str(&body).expect("model lookup response should be valid JSON");
        assert_eq!(
            json["id"],
            serde_json::Value::String("gemini-3-flash".to_string())
        );
        assert_eq!(json["type"], serde_json::Value::String("model".to_string()));
        assert_eq!(
            json["display_name"],
            serde_json::Value::String("gemini-3-flash".to_string())
        );
    }

    #[tokio::test]
    async fn test_model_by_id_endpoint_alias() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /v1/models/flash HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");

        let json: serde_json::Value =
            serde_json::from_str(&body).expect("model alias lookup response should be valid JSON");
        assert_eq!(
            json["id"],
            serde_json::Value::String("gemini-3-flash".to_string())
        );
    }

    #[tokio::test]
    async fn test_model_by_id_endpoint_no_v1_alias() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /models/flash HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");

        let json: serde_json::Value =
            serde_json::from_str(&body).expect("model alias lookup response should be valid JSON");
        assert_eq!(
            json["id"],
            serde_json::Value::String("gemini-3-flash".to_string())
        );
    }

    #[tokio::test]
    async fn test_model_by_id_not_found_openai_error_shape() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /v1/models/does-not-exist HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 404, "body: {body}");

        let json: serde_json::Value =
            serde_json::from_str(&body).expect("model not-found response should be valid JSON");
        assert!(
            json.get("error").is_some(),
            "expected OpenAI error wrapper, body: {body}"
        );
        assert_eq!(
            json["error"]["type"],
            serde_json::Value::String("invalid_request_error".to_string()),
            "expected OpenAI invalid_request_error type, body: {body}"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::Value::Null,
            "expected OpenAI code=null, body: {body}"
        );
        assert_eq!(
            json["error"]["param"],
            serde_json::Value::Null,
            "expected OpenAI param=null, body: {body}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .map(|m| m.contains("No model found with id"))
                .unwrap_or(false),
            "expected informative model-not-found message, body: {body}"
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

    // -- Token counting --

    #[tokio::test]
    async fn test_count_tokens() {
        let addr = spawn_test_server().await;
        let payload = r#"{"messages":[{"role":"user","content":"Hello, world!"}]}"#;
        let req = format!(
            "POST /v1/messages/count_tokens HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("input_tokens"), "body: {body}");
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

    #[tokio::test]
    async fn test_event_logging_batch_v1_alias() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "POST /v1/api/event_logging/batch HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""status":"ok"#), "body: {body}");
    }

    #[tokio::test]
    async fn test_event_logging_v1_alias() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "POST /v1/api/event_logging HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""status":"ok"#), "body: {body}");
    }

    #[tokio::test]
    async fn test_logs_stream_endpoint_sends_sse_and_heartbeat() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /api/logs/stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(
            body.contains("data:"),
            "expected SSE data frame, body: {body}"
        );
        assert!(
            body.contains(": ping"),
            "expected heartbeat ping, body: {body}"
        );
    }

    #[tokio::test]
    async fn test_openai_completions_alias_route() {
        let addr = spawn_test_server().await;
        let payload =
            r#"{"model":"gemini-3-flash","messages":[{"role":"user","content":"hello"}]}"#;
        let req = format!(
            "POST /v1/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(
            status, 401,
            "expected auth failure with no accounts, proving route is wired"
        );
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("OpenAI error response should be valid JSON");
        assert!(
            json.get("error").is_some(),
            "expected OpenAI error wrapper, body: {body}"
        );
        assert_eq!(
            json["error"]["type"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected OpenAI error type, body: {body}"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::Value::Null,
            "expected OpenAI code=null, body: {body}"
        );
        assert_eq!(
            json["error"]["param"],
            serde_json::Value::Null,
            "expected OpenAI param=null, body: {body}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .map(|m| m.contains("No enabled accounts available"))
                .unwrap_or(false),
            "expected informative authentication message, body: {body}"
        );
        assert!(
            !body.contains(r#""request_id""#),
            "OpenAI errors should not include Anthropic request_id body field: {body}"
        );
    }

    #[tokio::test]
    async fn test_openai_chat_completions_no_v1_alias_route() {
        let addr = spawn_test_server().await;
        let payload =
            r#"{"model":"gemini-3-flash","messages":[{"role":"user","content":"hello"}]}"#;
        let req = format!(
            "POST /chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(
            status, 401,
            "expected auth failure with no accounts, proving route is wired"
        );
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("OpenAI error response should be valid JSON");
        assert!(
            json.get("error").is_some(),
            "expected OpenAI error wrapper, body: {body}"
        );
        assert_eq!(
            json["error"]["type"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected OpenAI error type, body: {body}"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::Value::Null,
            "expected OpenAI code=null, body: {body}"
        );
        assert_eq!(
            json["error"]["param"],
            serde_json::Value::Null,
            "expected OpenAI param=null, body: {body}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .map(|m| m.contains("No enabled accounts available"))
                .unwrap_or(false),
            "expected informative authentication message, body: {body}"
        );
    }

    #[tokio::test]
    async fn test_legacy_completions_prompt_route() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"gemini-3-flash","prompt":"hello from completions"}"#;
        let req = format!(
            "POST /v1/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(
            status, 401,
            "expected auth failure with no accounts, proving prompt-style completions payload is accepted"
        );
        let json: serde_json::Value = serde_json::from_str(&body)
            .expect("Legacy completions error response should be valid JSON");
        assert!(
            json.get("error").is_some(),
            "expected OpenAI error wrapper, body: {body}"
        );
        assert_eq!(
            json["error"]["type"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected OpenAI error type, body: {body}"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::Value::Null,
            "expected OpenAI code=null, body: {body}"
        );
        assert_eq!(
            json["error"]["param"],
            serde_json::Value::Null,
            "expected OpenAI param=null, body: {body}"
        );
    }

    #[tokio::test]
    async fn test_legacy_completions_prompt_empty_rejected() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"gemini-3-flash","prompt":"   "}"#;
        let req = format!(
            "POST /v1/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "body: {body}");
        assert!(
            body.contains("prompt cannot be empty"),
            "expected prompt validation message, body: {body}"
        );
    }

    #[tokio::test]
    async fn test_legacy_completions_prompt_stream_route() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"gemini-3-flash","prompt":"hello","stream":true}"#;
        let req = format!(
            "POST /v1/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(
            status, 401,
            "expected auth failure with no accounts, proving prompt-style stream payload is accepted"
        );
        assert!(
            body.contains("No enabled accounts available"),
            "expected authentication error body, got: {body}"
        );
    }

    #[test]
    fn test_convert_chat_chunk_to_legacy_chunk_text_delta() {
        let chat_chunk = crate::format::openai::ChatCompletionChunk {
            id: "chatcmpl_abc".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234,
            model: "gemini-3-flash".to_string(),
            choices: vec![crate::format::openai::ChunkChoice {
                index: 0,
                delta: crate::format::openai::ChunkDelta {
                    role: Some("assistant".to_string()),
                    content: Some("hello".to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
            system_fingerprint: None,
        };

        let legacy = convert_chat_chunk_to_legacy_chunk(chat_chunk);
        assert_eq!(legacy.object, "text_completion.chunk");
        assert_eq!(legacy.choices.len(), 1);
        assert_eq!(legacy.choices[0].text, "hello");
        assert_eq!(legacy.choices[0].index, 0);
        assert!(legacy.choices[0].finish_reason.is_none());
    }

    #[test]
    fn test_convert_chat_chunk_to_legacy_chunk_usage_and_finish_reason() {
        let chat_chunk = crate::format::openai::ChatCompletionChunk {
            id: "chatcmpl_xyz".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 5678,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![crate::format::openai::ChunkChoice {
                index: 0,
                delta: crate::format::openai::ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage: Some(crate::format::openai::ChatUsage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
            }),
            system_fingerprint: Some("fp_test".to_string()),
        };

        let legacy = convert_chat_chunk_to_legacy_chunk(chat_chunk);
        assert_eq!(legacy.object, "text_completion.chunk");
        assert_eq!(legacy.choices[0].text, "");
        assert_eq!(legacy.choices[0].finish_reason, Some("stop".to_string()));
        assert_eq!(legacy.usage.as_ref().map(|u| u.total_tokens), Some(12));
        assert_eq!(legacy.system_fingerprint.as_deref(), Some("fp_test"));
    }

    #[tokio::test]
    async fn test_legacy_completions_stream_adapter_end_to_end() {
        use crate::format::openai::{ChatCompletionChunk, ChatUsage, ChunkChoice, ChunkDelta};
        use http_body_util::BodyExt;

        let (tx, body) = streaming_body();
        let upstream = sse_streaming_response(body, "req_chat_stream");

        tokio::spawn(async move {
            let first = ChatCompletionChunk {
                id: "chatcmpl_stream".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1700000000,
                model: "gemini-3-flash".to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: Some("assistant".to_string()),
                        content: Some("hello".to_string()),
                        tool_calls: None,
                    },
                    finish_reason: None,
                    logprobs: None,
                }],
                usage: None,
                system_fingerprint: None,
            };
            let second = ChatCompletionChunk {
                id: "chatcmpl_stream".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1700000000,
                model: "gemini-3-flash".to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: None,
                        tool_calls: None,
                    },
                    finish_reason: Some("stop".to_string()),
                    logprobs: None,
                }],
                usage: Some(ChatUsage {
                    prompt_tokens: 3,
                    completion_tokens: 2,
                    total_tokens: 5,
                }),
                system_fingerprint: Some("fp_stream".to_string()),
            };

            let _ = tx
                .send(Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&first).unwrap_or_default()
                )))
                .await;
            let _ = tx.send(Bytes::from(": ping\n\n")).await;
            let _ = tx
                .send(Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&second).unwrap_or_default()
                )))
                .await;
            let _ = tx.send(Bytes::from("data: [DONE]\n\n")).await;
        });

        let adapted = adapt_chat_stream_to_legacy_completions(upstream, "req_legacy_stream")
            .await
            .expect("adapter should build streaming response");

        assert_eq!(adapted.status(), StatusCode::OK);
        assert_eq!(
            adapted
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );

        let collected = adapted
            .into_body()
            .collect()
            .await
            .expect("collect adapted stream");
        let body = String::from_utf8_lossy(&collected.to_bytes()).to_string();

        assert!(
            body.contains(r#""object":"text_completion.chunk""#),
            "expected legacy completion chunk object, body: {body}"
        );
        assert!(
            !body.contains(r#""object":"chat.completion.chunk""#),
            "should not leak chat chunk object shape, body: {body}"
        );
        assert!(body.contains(r#""text":"hello""#), "body: {body}");
        assert!(body.contains(r#""finish_reason":"stop""#), "body: {body}");
        assert!(body.contains(r#""total_tokens":5"#), "body: {body}");
        assert!(body.contains(": ping"), "body: {body}");
        assert!(body.contains("data: [DONE]"), "body: {body}");
    }

    #[tokio::test]
    async fn test_responses_route_uses_responses_error_shape() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"gemini-3-flash","input":"hello"}"#;
        let req = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 401, "body: {body}");
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("Responses error response should be valid JSON");
        assert!(
            json.get("error").is_some(),
            "expected Responses error wrapper, body: {body}"
        );
        assert_eq!(
            json["error"]["type"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected Responses error type, body: {body}"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected Responses code=type, body: {body}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .map(|m| m.contains("No enabled accounts available"))
                .unwrap_or(false),
            "expected informative authentication message, body: {body}"
        );
        assert!(
            !body.contains(r#""request_id""#),
            "Responses API errors should not include Anthropic request_id body field: {body}"
        );
    }

    #[tokio::test]
    async fn test_responses_alias_route_uses_responses_error_shape() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"gemini-3-flash","input":"hello"}"#;
        let req = format!(
            "POST /v1/chat/completions/responses HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 401, "body: {body}");
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("Responses alias error should be valid JSON");
        assert!(
            json.get("error").is_some(),
            "expected Responses error wrapper, body: {body}"
        );
        assert_eq!(
            json["error"]["type"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected Responses error type, body: {body}"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected Responses code=type, body: {body}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .map(|m| m.contains("No enabled accounts available"))
                .unwrap_or(false),
            "expected informative authentication message, body: {body}"
        );
        assert!(
            !body.contains(r#""request_id""#),
            "Responses API errors should not include Anthropic request_id body field: {body}"
        );
    }

    #[tokio::test]
    async fn test_responses_no_v1_alias_route_uses_responses_error_shape() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"gemini-3-flash","input":"hello"}"#;
        let req = format!(
            "POST /responses HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 401, "body: {body}");
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("Responses alias error should be valid JSON");
        assert!(
            json.get("error").is_some(),
            "expected Responses error wrapper, body: {body}"
        );
        assert_eq!(
            json["error"]["type"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected Responses error type, body: {body}"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::Value::String("authentication_error".to_string()),
            "expected Responses code=type, body: {body}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .map(|m| m.contains("No enabled accounts available"))
                .unwrap_or(false),
            "expected informative authentication message, body: {body}"
        );
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

    // -- Internal warmup --

    #[tokio::test]
    async fn test_internal_warmup_no_accounts() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"gemini-3-flash"}"#;
        let req = format!(
            "POST /internal/warmup HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, _body) = http_request(addr, &req).await;
        assert_eq!(status, 503, "expected 503 when no accounts are configured");
    }

    // -- Native Gemini API --

    #[tokio::test]
    async fn test_gemini_models_endpoint() {
        let addr = spawn_test_server().await;
        let (status, body) = http_request(
            addr,
            "GET /v1beta/models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200, "body: {body}");
        assert!(
            body.contains("models/gemini-3-flash"),
            "body should list Gemini models: {body}"
        );
    }

    #[tokio::test]
    async fn test_gemini_count_tokens_endpoint() {
        let addr = spawn_test_server().await;
        let payload = r#"{"contents":[{"role":"user","parts":[{"text":"Hello from Gemini"}]}]}"#;
        let req = format!(
            "POST /v1beta/models/gemini-3-flash:countTokens HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("totalTokens"), "body: {body}");
    }

    #[tokio::test]
    async fn test_models_detect_endpoint() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"flash"}"#;
        let req = format!(
            "POST /v1/models/detect HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""model":"flash""#), "body: {body}");
        assert!(
            body.contains(r#""mapped_model":"gemini-3-flash""#),
            "body: {body}"
        );
        assert!(body.contains(r#""type":"gemini""#), "body: {body}");
    }

    #[tokio::test]
    async fn test_models_detect_missing_model() {
        let addr = spawn_test_server().await;
        let payload = r#"{}"#;
        let req = format!(
            "POST /v1/models/detect HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, _body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "expected 400 when model is missing");
    }

    // -- OpenAI Images / Audio --

    #[tokio::test]
    async fn test_images_generation_missing_prompt() {
        let addr = spawn_test_server().await;
        let payload = r#"{}"#;
        let req = format!(
            "POST /v1/images/generations HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, _body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "expected 400 when prompt is missing");
    }

    #[tokio::test]
    async fn test_images_edits_missing_image() {
        let addr = spawn_test_server().await;
        let boundary = "---------------------------agcp-test-boundary-edit-missing";
        let body = build_multipart_body_with_files(
            boundary,
            &[("prompt", "make it brighter")],
            &[("mask", "mask.png", "image/png", "MASKDATA")],
        );
        let req = format!(
            "POST /v1/images/edits HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            boundary,
            body.len(),
            body
        );
        let (status, _body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "expected 400 when image is missing");
    }

    #[tokio::test]
    async fn test_images_edits_mock_success() {
        let addr = spawn_test_server().await;
        let boundary = "---------------------------agcp-test-boundary-edit";
        let body = build_multipart_body_with_files(
            boundary,
            &[
                ("prompt", "make it brighter"),
                ("response_format", "b64_json"),
                ("n", "1"),
            ],
            &[("image", "input.png", "image/png", "IMAGEBYTES")],
        );
        let req = format!(
            "POST /v1/images/edits HTTP/1.1\r\nHost: localhost\r\nX-AGCP-Mock-Upstream: 1\r\nContent-Type: multipart/form-data; boundary={}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            boundary,
            body.len(),
            body
        );
        let (status, response_body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {response_body}");
        assert!(
            response_body.contains("MOCK_IMAGE_DATA_1"),
            "body: {response_body}"
        );
    }

    #[tokio::test]
    async fn test_images_variations_mock_success() {
        let addr = spawn_test_server().await;
        let boundary = "---------------------------agcp-test-boundary-variation";
        let body = build_multipart_body_with_files(
            boundary,
            &[("response_format", "url"), ("n", "1")],
            &[("image", "input.png", "image/png", "IMAGEBYTES")],
        );
        let req = format!(
            "POST /v1/images/variations HTTP/1.1\r\nHost: localhost\r\nX-AGCP-Mock-Upstream: 1\r\nContent-Type: multipart/form-data; boundary={}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            boundary,
            body.len(),
            body
        );
        let (status, response_body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {response_body}");
        assert!(
            response_body.contains("data:image/png;base64"),
            "body: {response_body}"
        );
    }

    #[tokio::test]
    async fn test_audio_transcriptions_requires_multipart() {
        let addr = spawn_test_server().await;
        let payload = r#"{}"#;
        let req = format!(
            "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, _body) = http_request(addr, &req).await;
        assert_eq!(status, 400, "expected 400 for non-multipart audio request");
    }

    // -- Mocked upstream success paths --

    #[tokio::test]
    async fn test_gemini_generate_content_mock_success() {
        let addr = spawn_test_server().await;
        let payload = r#"{"contents":[{"role":"user","parts":[{"text":"Hello Gemini"}]}]}"#;
        let req = format!(
            "POST /v1beta/models/gemini-3-flash:generateContent HTTP/1.1\r\nHost: localhost\r\nX-AGCP-Mock-Upstream: 1\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("mock-gemini-response"), "body: {body}");
    }

    #[tokio::test]
    async fn test_gemini_stream_generate_content_mock_success() {
        let addr = spawn_test_server().await;
        let payload = r#"{"contents":[{"role":"user","parts":[{"text":"Hello Gemini stream"}]}]}"#;
        let req = format!(
            "POST /v1beta/models/gemini-3-flash:streamGenerateContent HTTP/1.1\r\nHost: localhost\r\nX-AGCP-Mock-Upstream: 1\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("data:"), "body: {body}");
        assert!(body.contains("mock stream chunk"), "body: {body}");
    }

    #[tokio::test]
    async fn test_images_generation_mock_success_with_revised_prompt() {
        let addr = spawn_test_server().await;
        let payload =
            r#"{"prompt":"A watercolor fox in a city","response_format":"b64_json","n":2}"#;
        let req = format!(
            "POST /v1/images/generations HTTP/1.1\r\nHost: localhost\r\nX-AGCP-Mock-Upstream: 1\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("MOCK_IMAGE_DATA_1"), "body: {body}");
        assert!(body.contains("revised_prompt"), "body: {body}");
    }

    #[tokio::test]
    async fn test_audio_transcriptions_mock_success_json() {
        let addr = spawn_test_server().await;
        let boundary = "---------------------------agcp-test-boundary";
        let body = build_multipart_body(
            boundary,
            &[
                ("model", "gemini-3-flash"),
                ("response_format", "json"),
                ("language", "en"),
            ],
            "sample.wav",
            "audio/wav",
            "RIFFMOCKAUDIO",
        );
        let req = format!(
            "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: localhost\r\nX-AGCP-Mock-Upstream: 1\r\nContent-Type: multipart/form-data; boundary={}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            boundary,
            body.len(),
            body
        );
        let (status, response_body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {response_body}");
        assert!(
            response_body.contains("mock transcription from agcp test upstream"),
            "body: {response_body}"
        );
    }

    #[tokio::test]
    async fn test_audio_transcriptions_mock_success_vtt() {
        let addr = spawn_test_server().await;
        let boundary = "---------------------------agcp-test-boundary-vtt";
        let body = build_multipart_body(
            boundary,
            &[
                ("model", "gemini-3-flash"),
                ("response_format", "vtt"),
                ("timestamp_granularities[]", "word"),
                ("timestamp_granularities[]", "segment"),
            ],
            "sample.wav",
            "audio/wav",
            "RIFFMOCKAUDIO",
        );
        let req = format!(
            "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: localhost\r\nX-AGCP-Mock-Upstream: 1\r\nContent-Type: multipart/form-data; boundary={}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            boundary,
            body.len(),
            body
        );
        let (status, response_body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {response_body}");
        assert!(response_body.contains("WEBVTT"), "body: {response_body}");
        assert!(
            response_body.contains("mock transcription from agcp test upstream"),
            "body: {response_body}"
        );
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
        let payload = r#"{"model":"claude-sonnet-4-6-thinking","max_tokens":100,"messages":[]}"#;
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
        let payload = r#"{"model":"claude-sonnet-4-6-thinking","max_tokens":0,"messages":[{"role":"user","content":"hi"}]}"#;
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
        let payload = r#"{"model":"claude-sonnet-4-6-thinking","max_tokens":999999,"messages":[{"role":"user","content":"hi"}]}"#;
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

    #[tokio::test]
    async fn test_messages_warmup_intercept_non_stream() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"claude-sonnet-4-6-thinking","max_tokens":8,"stream":false,"messages":[{"role":"user","content":"Warmup ping"}]}"#;
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains(r#""text":"OK""#), "body: {body}");
    }

    #[tokio::test]
    async fn test_messages_warmup_intercept_stream() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"claude-sonnet-4-6-thinking","max_tokens":8,"stream":true,"messages":[{"role":"user","content":"Warmup ping"}]}"#;
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("event: message_start"), "body: {body}");
        assert!(body.contains("event: message_stop"), "body: {body}");
    }

    #[tokio::test]
    async fn test_messages_non_warmup_not_intercepted_without_accounts() {
        let addr = spawn_test_server().await;
        let payload = r#"{"model":"claude-sonnet-4-6-thinking","max_tokens":8,"stream":false,"messages":[{"role":"user","content":"continue"}]}"#;
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let (status, body) = http_request(addr, &req).await;
        assert_eq!(
            status, 401,
            "expected auth/account failure for non-warmup messages with no accounts"
        );
        assert!(
            body.contains(r#""type":"error""#)
                && body.contains(r#""type":"authentication_error""#)
                && body.contains(r#""request_id""#),
            "expected Anthropic error shape, body: {body}"
        );
    }
}
