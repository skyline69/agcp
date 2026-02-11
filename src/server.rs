use http_body_util::{BodyExt, Either, Full};
use hyper::body::{Bytes, Frame};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
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
use crate::format::{
    ChatCompletionRequest, MessagesRequest, ModelInfo, ModelsResponse, StreamEvent,
};
use crate::models::{Model, get_fallback_model, is_thinking_model, resolve_with_mappings};
use crate::stats::get_stats;

/// Maximum request body size (10 MB).
const MAX_REQUEST_SIZE: usize = 10 * 1024 * 1024;

/// Maximum time to wait for a single upstream frame before considering the
/// stream stalled (seconds).
const STREAM_FRAME_TIMEOUT_SECS: u64 = 300;

/// Channel buffer size for streaming SSE responses.
///
/// Sized to allow the upstream parser to stay ahead of the client without
/// unbounded memory growth.  Each item is a small SSE text frame.
const STREAM_CHANNEL_BUFFER: usize = 64;

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

            // Token counting API — estimates token count using chars/4 heuristic
            (Method::POST, "/v1/messages/count_tokens") => {
                handle_count_tokens(req).await
            }

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
            let resp = error_to_response(&e, &request_id);
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
            | "/api/event_logging/batch"
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
    let result = execute_openai_request(&messages_request, &state, request_id, false).await;

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
) -> Result<Response<ResponseBody>, Error> {
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

        loop {
            use http_body_util::BodyExt;
            let frame_timeout = Duration::from_secs(STREAM_FRAME_TIMEOUT_SECS);
            match tokio::time::timeout(frame_timeout, incoming.frame()).await {
                Ok(Some(Ok(frame))) => {
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
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "Error reading upstream for OpenAI streaming");
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    warn!("Upstream frame timeout in OpenAI streaming");
                    break;
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
        loop {
            use http_body_util::BodyExt;
            let frame_timeout = Duration::from_secs(STREAM_FRAME_TIMEOUT_SECS);
            match tokio::time::timeout(frame_timeout, incoming.frame()).await {
                Ok(Some(Ok(frame))) => {
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
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "Error reading upstream for Responses streaming");
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    warn!("Upstream frame timeout in Responses streaming");
                    break;
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
) -> Response<ResponseBody> {
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
        .body(full_body(Full::new(Bytes::from(body))))
        .expect("Response construction with valid headers should not fail")
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

/// Estimate token count for a messages request.
///
/// Uses a chars/4 heuristic which is a reasonable approximation for most
/// tokenizers (GPT, Claude, Gemini all average ~3.5-4.5 chars per token
/// for English text). This avoids requiring a full tokenizer dependency.
async fn handle_count_tokens(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<ResponseBody>, Error> {
    let body_bytes = read_body_limited(req.into_body(), MAX_REQUEST_SIZE).await?;

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
        .body(full_body(Full::new(Bytes::from(response.to_string()))))
        .unwrap())
}

async fn handle_logs_stream() -> Result<Response<ResponseBody>, Error> {
    // Get the log file path
    let config_dir = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config").join("agcp"))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log_path = config_dir.join("agcp.log");

    // Read the last 100 lines from the log file efficiently (seek from end)
    let lines = match std::fs::File::open(&log_path) {
        Ok(mut file) => {
            use std::io::{Read, Seek, SeekFrom};
            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len == 0 {
                vec!["No log entries".to_string()]
            } else {
                const TAIL_COUNT: usize = 100;
                const CHUNK_SIZE: u64 = 64 * 1024;
                let mut collected: Vec<String> = Vec::new();
                let mut remaining = file_len;

                while remaining > 0 && collected.len() < TAIL_COUNT + 1 {
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

                let start = collected.len().saturating_sub(TAIL_COUNT);
                collected[start..].to_vec()
            }
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
        .body(full_body(Full::new(Bytes::from(body))))
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

fn error_to_response(error: &Error, request_id: &str) -> Response<ResponseBody> {
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
        .body(full_body(Full::new(Bytes::from(body))))
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
            body.contains("gemini-3-flash"),
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
