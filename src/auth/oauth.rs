use base64::Engine;
use hyper::Response;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use super::token::{CLIENT_ID, CLIENT_SECRET, TOKEN_URL};
use crate::error::{AuthError, Error, Result};

type CallbackSender =
    Arc<tokio::sync::Mutex<Option<oneshot::Sender<std::result::Result<String, String>>>>>;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const CALLBACK_PORT: u16 = 51121;
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let mut verifier_bytes = [0u8; 32];
        getrandom::fill(&mut verifier_bytes).expect("Failed to generate random bytes");

        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge_bytes = hasher.finalize();
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge_bytes);

        Self {
            verifier,
            challenge,
        }
    }
}

pub fn generate_state() -> String {
    let mut state_bytes = [0u8; 16];
    getrandom::fill(&mut state_bytes).expect("Failed to generate random bytes");

    hex_encode(&state_bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn get_authorization_url(redirect_uri: &str) -> (String, Pkce, String) {
    let pkce = Pkce::generate();
    let state = generate_state();

    let scope = SCOPES.join(" ");
    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&code_challenge={}&code_challenge_method=S256&state={}",
        AUTH_URL,
        percent_encode(CLIENT_ID),
        percent_encode(redirect_uri),
        percent_encode(&scope),
        percent_encode(&pkce.challenge),
        percent_encode(&state),
    );

    (url, pkce, state)
}

fn percent_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

pub async fn exchange_code(
    http_client: &super::HttpClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<(String, String, u64)> {
    debug!("Exchanging authorization code for tokens");

    let body = format!(
        "client_id={}&client_secret={}&code={}&code_verifier={}&grant_type=authorization_code&redirect_uri={}",
        CLIENT_ID,
        CLIENT_SECRET,
        percent_encode(code),
        percent_encode(verifier),
        percent_encode(redirect_uri),
    );

    let response = http_client
        .post(
            TOKEN_URL,
            "application/x-www-form-urlencoded",
            body.as_bytes(),
        )
        .await
        .map_err(|e| {
            Error::Auth(AuthError::OAuthFailed(format!(
                "token exchange failed: {}",
                e
            )))
        })?;

    #[derive(serde::Deserialize)]
    struct TokenResponse {
        access_token: String,
        refresh_token: String,
        expires_in: u64,
    }

    let tokens: TokenResponse = serde_json::from_slice(&response).map_err(|e| {
        Error::Auth(AuthError::OAuthFailed(format!(
            "invalid token response: {}",
            e
        )))
    })?;

    debug!("Successfully obtained tokens");
    Ok((tokens.access_token, tokens.refresh_token, tokens.expires_in))
}

pub async fn start_callback_server(
    expected_state: String,
) -> Result<(u16, oneshot::Receiver<std::result::Result<String, String>>)> {
    use hyper::Request;
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use std::net::SocketAddr;

    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
    let expected_state = Arc::new(expected_state);

    let addr: SocketAddr = ([127, 0, 0, 1], CALLBACK_PORT).into();
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        Error::Auth(AuthError::OAuthFailed(format!(
            "failed to bind callback server on port {}: {}",
            CALLBACK_PORT, e
        )))
    })?;

    let actual_port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(CALLBACK_PORT);

    debug!(port = actual_port, "OAuth callback server started");

    let tx_clone = tx.clone();
    let expected_state_clone = expected_state.clone();

    tokio::spawn(async move {
        let timeout = tokio::time::sleep(CALLBACK_TIMEOUT);
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                _ = &mut timeout => {
                    warn!("OAuth callback server timed out");
                    break;
                }
                result = listener.accept() => {
                    let (stream, remote_addr) = match result {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(error = %e, "Failed to accept connection");
                            continue;
                        }
                    };

                    debug!(remote = %remote_addr, "Received callback connection");

                    let io = TokioIo::new(stream);
                    let tx = tx_clone.clone();
                    let expected_state = expected_state_clone.clone();

                    tokio::spawn(async move {
                        let service = service_fn(move |req: Request<Incoming>| {
                            let tx = tx.clone();
                            let expected_state = expected_state.clone();
                            async move { handle_callback(req, tx, &expected_state).await }
                        });

                        if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                            debug!(error = %e, "Callback connection error");
                        }
                    });
                }
            }
        }
    });

    Ok((actual_port, rx))
}

async fn handle_callback(
    req: hyper::Request<hyper::body::Incoming>,
    tx: CallbackSender,
    expected_state: &str,
) -> std::result::Result<Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
    use http_body_util::Full;
    use hyper::body::Bytes;

    let path = req.uri().path();
    if path != "/oauth-callback" {
        return Ok(Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("Not found")))
            .unwrap());
    }

    let query = req.uri().query().unwrap_or("");
    let params = parse_query(query);

    let mut guard = tx.lock().await;

    if let Some(error) = params.get("error") {
        let html = format!(
            r#"<!DOCTYPE html>
<html><head><title>Authentication Failed</title></head>
<body style="font-family: system-ui; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">Authentication Failed</h1>
<p>Error: {}</p>
<p>You can close this window.</p>
</body></html>"#,
            error
        );

        if let Some(sender) = guard.take() {
            let _ = sender.send(Err(error.clone()));
        }

        return Ok(Response::builder()
            .status(400)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(html)))
            .unwrap());
    }

    let state = params.get("state").map(|s| s.as_str()).unwrap_or("");
    if state != expected_state {
        let html = r#"<!DOCTYPE html>
<html><head><title>Authentication Failed</title></head>
<body style="font-family: system-ui; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">Authentication Failed</h1>
<p>State mismatch - possible CSRF attack.</p>
<p>You can close this window.</p>
</body></html>"#;

        if let Some(sender) = guard.take() {
            let _ = sender.send(Err("State mismatch".to_string()));
        }

        return Ok(Response::builder()
            .status(400)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(html)))
            .unwrap());
    }

    let code = match params.get("code") {
        Some(c) => c.clone(),
        None => {
            let html = r#"<!DOCTYPE html>
<html><head><title>Authentication Failed</title></head>
<body style="font-family: system-ui; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">Authentication Failed</h1>
<p>No authorization code received.</p>
<p>You can close this window.</p>
</body></html>"#;

            if let Some(sender) = guard.take() {
                let _ = sender.send(Err("No code".to_string()));
            }

            return Ok(Response::builder()
                .status(400)
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Full::new(Bytes::from(html)))
                .unwrap());
        }
    };

    let html = r#"<!DOCTYPE html>
<html><head><title>Authentication Successful</title></head>
<body style="font-family: system-ui; padding: 40px; text-align: center;">
<h1 style="color: #28a745;">Authentication Successful</h1>
<p>You can close this window and return to the terminal.</p>
<script>setTimeout(() => window.close(), 2000);</script>
</body></html>"#;

    if let Some(sender) = guard.take() {
        let _ = sender.send(Ok(code));
    }

    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)))
        .unwrap())
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            match (parts.next(), parts.next()) {
                (Some(k), Some(v)) => Some((percent_decode(k), percent_decode(v))),
                _ => None,
            }
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2)
                && let Ok(byte) = u8::from_str_radix(&format!("{}{}", h1, h2), 16)
            {
                result.push(byte as char);
                continue;
            }
        } else if c == '+' {
            result.push(' ');
            continue;
        }
        result.push(c);
    }
    result
}
