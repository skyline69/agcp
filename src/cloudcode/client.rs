use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore, SemaphorePermit};
use tracing::{debug, info, warn};

use crate::config::CloudCodeConfig;
use crate::error::{ApiError, Error, Result};
use crate::format::google::GenerateContentResponse;

use super::rate_limit::{
    CAPACITY_BACKOFF_TIERS_MS, DEFAULT_COOLDOWN_MS, FIRST_RETRY_DELAY_MS, MAX_CAPACITY_RETRIES,
    MAX_WAIT_BEFORE_ERROR_MS, calculate_smart_backoff, clear_rate_limit_state,
    get_rate_limit_backoff, is_model_capacity_exhausted, parse_reset_time,
};

/// Google Cloud Code API endpoints (daily and production).
pub const ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];

/// HTTP client for Google Cloud Code API with retry logic and rate limiting.
///
/// Features:
/// - Dual endpoint failover (daily-cloudcode → cloudcode)
/// - Exponential backoff for 429 rate limits
/// - Configurable timeouts and retry limits
/// - Request throttling via semaphore
pub struct CloudCodeClient {
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
    request_semaphore: Arc<Semaphore>,
    last_request_time: Mutex<std::time::Instant>,
    api_timeout: Duration,
    max_retries: u32,
    min_request_interval: Duration,
}

impl CloudCodeClient {
    /// Create a new Cloud Code client with the given configuration.
    pub fn new(config: &CloudCodeConfig) -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .enable_http2()
            .build();

        let client = Client::builder(TokioExecutor::new()).build(connector);

        Self {
            client,
            request_semaphore: Arc::new(Semaphore::new(config.max_concurrent_requests)),
            last_request_time: Mutex::new(std::time::Instant::now()),
            api_timeout: Duration::from_secs(config.timeout_secs),
            max_retries: config.max_retries,
            min_request_interval: Duration::from_millis(config.min_request_interval_ms),
        }
    }

    async fn acquire_request_permit(&self) -> Result<SemaphorePermit<'_>> {
        let permit = self
            .request_semaphore
            .acquire()
            .await
            .map_err(|_| Error::Http("Request semaphore closed".into()))?;

        {
            let mut last_time = self.last_request_time.lock().await;
            let elapsed = last_time.elapsed();

            if elapsed < self.min_request_interval {
                let wait_time = self.min_request_interval - elapsed;
                tokio::time::sleep(wait_time).await;
            }

            *last_time = std::time::Instant::now();
        }

        Ok(permit)
    }

    pub async fn send_request(
        &self,
        body: Bytes,
        access_token: &str,
        model: &str,
    ) -> Result<GenerateContentResponse> {
        let _permit = self.acquire_request_permit().await?;

        let headers = super::request::build_headers(access_token, model, false);
        let start_time = std::time::Instant::now();

        let mut last_error = None;
        let mut capacity_retry_count = 0u32;

        for (i, endpoint) in ENDPOINTS.iter().enumerate() {
            let url = format!("{endpoint}/v1internal:generateContent");

            debug!(endpoint = %endpoint, attempt = i + 1, "Sending request to Cloud Code API");

            let mut retry_count = 0u32;

            loop {
                let elapsed_ms = start_time.elapsed().as_millis() as u64;
                if elapsed_ms > MAX_WAIT_BEFORE_ERROR_MS {
                    warn!(
                        elapsed_ms = elapsed_ms,
                        max_wait_ms = MAX_WAIT_BEFORE_ERROR_MS,
                        "Max total wait time exceeded"
                    );
                    if let Some(Error::Api(ApiError::QuotaExhausted {
                        model: m,
                        reset_time: r,
                    })) = &last_error
                    {
                        return Err(Error::Api(ApiError::QuotaExhausted {
                            model: m.clone(),
                            reset_time: r.clone(),
                        }));
                    }
                    return Err(last_error.unwrap_or_else(|| {
                        Error::Api(ApiError::QuotaExhausted {
                            model: model.to_string(),
                            reset_time: "unknown".to_string(),
                        })
                    }));
                }

                match tokio::time::timeout(
                    self.api_timeout,
                    self.post(&url, &headers, body.clone()),
                )
                .await
                {
                    Ok(Ok(response_bytes)) => {
                        let response: GenerateContentResponse =
                            serde_json::from_slice(&response_bytes)
                                .map_err(|e| Error::Http(format!("Invalid response JSON: {e}")))?;

                        if let Some(error) = &response.error {
                            if error.code == 429 && retry_count < self.max_retries {
                                retry_count += 1;

                                let (wait_ms, reset_time_str) =
                                    parse_reset_time(&error.message, FIRST_RETRY_DELAY_MS);

                                // Check if this is a model capacity issue (not quota)
                                if is_model_capacity_exhausted(&error.message) {
                                    if capacity_retry_count < MAX_CAPACITY_RETRIES {
                                        let tier_index = (capacity_retry_count as usize)
                                            .min(CAPACITY_BACKOFF_TIERS_MS.len() - 1);
                                        let capacity_wait = CAPACITY_BACKOFF_TIERS_MS[tier_index];
                                        capacity_retry_count += 1;

                                        info!(
                                            retry = capacity_retry_count,
                                            max_retries = MAX_CAPACITY_RETRIES,
                                            wait_ms = capacity_wait,
                                            "Model capacity exhausted, retrying..."
                                        );

                                        tokio::time::sleep(Duration::from_millis(capacity_wait))
                                            .await;
                                        continue;
                                    }
                                    warn!("Max capacity retries exceeded, failing request");
                                }

                                if wait_ms > MAX_WAIT_BEFORE_ERROR_MS {
                                    warn!(
                                        model = %model,
                                        reset_time = %reset_time_str,
                                        "Quota exhausted with long reset time"
                                    );
                                    return Err(Error::Api(ApiError::QuotaExhausted {
                                        model: model.to_string(),
                                        reset_time: reset_time_str,
                                    }));
                                }

                                // Short rate limits (<1s) - always wait and retry immediately
                                if wait_ms < 1000 {
                                    info!(
                                        wait_ms = wait_ms,
                                        "Short rate limit, waiting and retrying..."
                                    );
                                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                                    continue;
                                }

                                // Deduplication prevents thundering herd on concurrent 429s
                                let backoff = get_rate_limit_backoff(model, Some(wait_ms));

                                if backoff.is_duplicate {
                                    debug!(
                                        model = %model,
                                        attempt = backoff.attempt,
                                        "Duplicate rate limit detected, skipping retry"
                                    );
                                }

                                let smart_backoff_ms =
                                    calculate_smart_backoff(&error.message, Some(wait_ms), 0);

                                // Quick retry on first 429 if backoff is short
                                let actual_wait = if backoff.attempt == 1
                                    && smart_backoff_ms <= DEFAULT_COOLDOWN_MS
                                {
                                    backoff.delay_ms
                                } else {
                                    smart_backoff_ms
                                };

                                let remaining_budget =
                                    MAX_WAIT_BEFORE_ERROR_MS.saturating_sub(elapsed_ms);
                                let actual_wait = actual_wait.min(remaining_budget);

                                if actual_wait == 0 {
                                    return Err(Error::Api(ApiError::QuotaExhausted {
                                        model: model.to_string(),
                                        reset_time: reset_time_str,
                                    }));
                                }

                                info!(
                                    endpoint = %endpoint,
                                    retry = retry_count,
                                    max_retries = self.max_retries,
                                    wait_ms = actual_wait,
                                    attempt = backoff.attempt,
                                    is_duplicate = backoff.is_duplicate,
                                    reset_time = %reset_time_str,
                                    "Rate limited (429), waiting before retry"
                                );

                                tokio::time::sleep(Duration::from_millis(actual_wait)).await;

                                last_error = Some(Error::Api(ApiError::QuotaExhausted {
                                    model: model.to_string(),
                                    reset_time: reset_time_str,
                                }));

                                continue;
                            }

                            let err = map_google_error(error.code, &error.message);
                            if matches!(
                                &err,
                                Error::Auth(_) | Error::Api(ApiError::InvalidRequest { .. })
                            ) {
                                return Err(err);
                            }
                            last_error = Some(err);
                            break;
                        }

                        clear_rate_limit_state(model);
                        return Ok(response);
                    }
                    Ok(Err(e)) => {
                        if let Error::Api(ApiError::RateLimited { .. }) = &e
                            && retry_count < self.max_retries
                        {
                            retry_count += 1;

                            let backoff = get_rate_limit_backoff(model, None);
                            let remaining_budget =
                                MAX_WAIT_BEFORE_ERROR_MS.saturating_sub(elapsed_ms);
                            let actual_wait = backoff.delay_ms.min(remaining_budget);

                            if actual_wait > 0 {
                                info!(
                                    endpoint = %endpoint,
                                    retry = retry_count,
                                    max_retries = self.max_retries,
                                    wait_ms = actual_wait,
                                    attempt = backoff.attempt,
                                    is_duplicate = backoff.is_duplicate,
                                    "Rate limited, waiting before retry"
                                );
                                tokio::time::sleep(Duration::from_millis(actual_wait)).await;
                                continue;
                            }
                        }

                        warn!(endpoint = %endpoint, error = %e, "Request failed, trying next endpoint");
                        last_error = Some(e);
                        break;
                    }
                    Err(_) => {
                        warn!(endpoint = %endpoint, "Request timed out, trying next endpoint");
                        last_error = Some(Error::Timeout(self.api_timeout));
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::Http("All endpoints failed".to_string())))
    }

    pub async fn send_streaming_request(
        &self,
        body: Bytes,
        access_token: &str,
        model: &str,
    ) -> Result<hyper::Response<hyper::body::Incoming>> {
        let _permit = self.acquire_request_permit().await?;

        let headers = super::request::build_headers(access_token, model, true);
        let start_time = std::time::Instant::now();

        let mut last_error = None;
        let mut capacity_retry_count = 0u32;

        for (i, endpoint) in ENDPOINTS.iter().enumerate() {
            let url = format!("{endpoint}/v1internal:streamGenerateContent?alt=sse");

            debug!(endpoint = %endpoint, attempt = i + 1, "Sending streaming request");

            let mut retry_count = 0u32;

            loop {
                let elapsed_ms = start_time.elapsed().as_millis() as u64;
                if elapsed_ms > MAX_WAIT_BEFORE_ERROR_MS {
                    warn!(
                        elapsed_ms = elapsed_ms,
                        max_wait_ms = MAX_WAIT_BEFORE_ERROR_MS,
                        "Max total wait time exceeded"
                    );
                    if let Some(Error::Api(ApiError::QuotaExhausted {
                        model: m,
                        reset_time: r,
                    })) = &last_error
                    {
                        return Err(Error::Api(ApiError::QuotaExhausted {
                            model: m.clone(),
                            reset_time: r.clone(),
                        }));
                    }
                    return Err(last_error.unwrap_or_else(|| {
                        Error::Api(ApiError::QuotaExhausted {
                            model: model.to_string(),
                            reset_time: "unknown".to_string(),
                        })
                    }));
                }

                match self.post_raw(&url, &headers, body.clone()).await {
                    Ok(response) => {
                        if response.status().is_success() {
                            clear_rate_limit_state(model);
                            return Ok(response);
                        }
                        let status = response.status().as_u16();

                        let error_body = http_body_util::BodyExt::collect(response.into_body())
                            .await
                            .ok()
                            .and_then(|b| String::from_utf8(b.to_bytes().to_vec()).ok())
                            .unwrap_or_default();
                        let error_preview: String = error_body.chars().take(500).collect();

                        if status == 429 && retry_count < self.max_retries {
                            retry_count += 1;

                            let (wait_ms, reset_time_str) =
                                parse_reset_time(&error_body, FIRST_RETRY_DELAY_MS);

                            if is_model_capacity_exhausted(&error_body) {
                                if capacity_retry_count < MAX_CAPACITY_RETRIES {
                                    let tier_index = (capacity_retry_count as usize)
                                        .min(CAPACITY_BACKOFF_TIERS_MS.len() - 1);
                                    let capacity_wait = CAPACITY_BACKOFF_TIERS_MS[tier_index];
                                    capacity_retry_count += 1;

                                    info!(
                                        retry = capacity_retry_count,
                                        max_retries = MAX_CAPACITY_RETRIES,
                                        wait_ms = capacity_wait,
                                        "Model capacity exhausted, retrying..."
                                    );

                                    tokio::time::sleep(Duration::from_millis(capacity_wait)).await;
                                    continue;
                                }
                                warn!("Max capacity retries exceeded, trying next endpoint");
                            }

                            if wait_ms > MAX_WAIT_BEFORE_ERROR_MS {
                                warn!(
                                    model = %model,
                                    reset_time = %reset_time_str,
                                    "Quota exhausted with long reset time"
                                );
                                return Err(Error::Api(ApiError::QuotaExhausted {
                                    model: model.to_string(),
                                    reset_time: reset_time_str,
                                }));
                            }

                            // Short rate limits (<1s) - always wait and retry immediately
                            if wait_ms < 1000 {
                                info!(
                                    wait_ms = wait_ms,
                                    "Short rate limit, waiting and retrying..."
                                );
                                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                                continue;
                            }

                            // Deduplication prevents thundering herd on concurrent 429s
                            let backoff = get_rate_limit_backoff(model, Some(wait_ms));

                            if backoff.is_duplicate {
                                debug!(
                                    model = %model,
                                    attempt = backoff.attempt,
                                    "Duplicate rate limit detected"
                                );
                            }

                            let smart_backoff_ms =
                                calculate_smart_backoff(&error_body, Some(wait_ms), 0);

                            // Quick retry on first 429 if backoff is short
                            let actual_wait = if backoff.attempt == 1
                                && smart_backoff_ms <= DEFAULT_COOLDOWN_MS
                            {
                                backoff.delay_ms
                            } else {
                                smart_backoff_ms
                            };

                            let remaining_budget =
                                MAX_WAIT_BEFORE_ERROR_MS.saturating_sub(elapsed_ms);
                            let actual_wait = actual_wait.min(remaining_budget);

                            if actual_wait == 0 {
                                return Err(Error::Api(ApiError::QuotaExhausted {
                                    model: model.to_string(),
                                    reset_time: reset_time_str,
                                }));
                            }

                            info!(
                                endpoint = %endpoint,
                                retry = retry_count,
                                max_retries = self.max_retries,
                                wait_ms = actual_wait,
                                attempt = backoff.attempt,
                                is_duplicate = backoff.is_duplicate,
                                reset_time = %reset_time_str,
                                error = %error_preview,
                                "Rate limited (429), waiting before retry"
                            );

                            tokio::time::sleep(Duration::from_millis(actual_wait)).await;

                            last_error = Some(Error::Api(ApiError::QuotaExhausted {
                                model: model.to_string(),
                                reset_time: reset_time_str,
                            }));

                            continue;
                        }

                        if status == 503
                            && is_model_capacity_exhausted(&error_body)
                            && capacity_retry_count < MAX_CAPACITY_RETRIES
                        {
                            let tier_index = (capacity_retry_count as usize)
                                .min(CAPACITY_BACKOFF_TIERS_MS.len() - 1);
                            let capacity_wait = CAPACITY_BACKOFF_TIERS_MS[tier_index];
                            capacity_retry_count += 1;

                            info!(
                                retry = capacity_retry_count,
                                max_retries = MAX_CAPACITY_RETRIES,
                                wait_ms = capacity_wait,
                                "503 Model capacity exhausted, retrying..."
                            );

                            tokio::time::sleep(Duration::from_millis(capacity_wait)).await;
                            continue;
                        }

                        warn!(
                            endpoint = %endpoint,
                            status = status,
                            model = %model,
                            error_body = %error_preview,
                            "Non-success status, trying next endpoint"
                        );
                        last_error = Some(map_http_error(status, &error_preview, Some(model)));
                        break;
                    }
                    Err(e) => {
                        warn!(endpoint = %endpoint, error = %e, "Streaming request failed");
                        last_error = Some(e);
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::Http("All endpoints failed".to_string())))
    }

    async fn post(
        &self,
        url: &str,
        headers: &[(Cow<'static, str>, Cow<'static, str>)],
        body: Bytes,
    ) -> Result<Bytes> {
        let response = self.post_raw(url, headers, body).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body_bytes = response
                .into_body()
                .collect()
                .await
                .map(|b| b.to_bytes())
                .unwrap_or_default();
            let message = String::from_utf8_lossy(&body_bytes).to_string();

            return Err(map_http_error(status, &message, None));
        }

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(body.to_bytes())
    }

    async fn post_raw(
        &self,
        url: &str,
        headers: &[(Cow<'static, str>, Cow<'static, str>)],
        body: Bytes,
    ) -> Result<hyper::Response<hyper::body::Incoming>> {
        let mut req = Request::builder().method("POST").uri(url);

        for (name, value) in headers {
            req = req.header(name.as_ref(), value.as_ref());
        }

        let req = req
            .body(Full::new(body))
            .map_err(|e| Error::Http(e.to_string()))?;

        self.client
            .request(req)
            .await
            .map_err(|e| Error::Http(e.to_string()))
    }
}

impl Default for CloudCodeClient {
    fn default() -> Self {
        Self::new(&crate::config::CloudCodeConfig::default())
    }
}

fn map_http_error(status: u16, message: &str, model: Option<&str>) -> Error {
    match status {
        401 => Error::Auth(crate::error::AuthError::TokenExpired),
        429 => {
            if let Some(model) = model {
                let (_, reset_time) = parse_reset_time(message, 60000);
                Error::Api(ApiError::QuotaExhausted {
                    model: model.to_string(),
                    reset_time,
                })
            } else {
                Error::Api(ApiError::RateLimited {
                    retry_after: Duration::from_secs(60),
                })
            }
        }
        400 => Error::Api(ApiError::InvalidRequest {
            message: message.to_string(),
        }),
        413 => Error::Api(ApiError::RequestTooLarge {
            size: 0,
            max: 10 * 1024 * 1024,
        }),
        500..=599 => Error::Api(ApiError::ServerError {
            status,
            message: message.to_string(),
        }),
        _ => Error::Http(["HTTP ", &status.to_string(), ": ", message].concat()),
    }
}

fn map_google_error(code: i32, message: &str) -> Error {
    match code {
        401 => Error::Auth(crate::error::AuthError::TokenExpired),
        429 => {
            if message.contains("RESOURCE_EXHAUSTED") || message.contains("quota") {
                Error::Api(ApiError::QuotaExhausted {
                    model: "unknown".to_string(),
                    reset_time: "unknown".to_string(),
                })
            } else {
                Error::Api(ApiError::RateLimited {
                    retry_after: Duration::from_secs(60),
                })
            }
        }
        400 => Error::Api(ApiError::InvalidRequest {
            message: message.to_string(),
        }),
        503 if message.contains("capacity") => Error::Api(ApiError::CapacityExhausted),
        _ => Error::Api(ApiError::ServerError {
            status: code as u16,
            message: message.to_string(),
        }),
    }
}
