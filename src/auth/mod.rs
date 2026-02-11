pub mod accounts;
pub mod oauth;
pub mod token;

pub use accounts::Account;
pub use oauth::{CALLBACK_PORT, exchange_code, get_authorization_url, start_callback_server};
pub use token::get_user_email;

use http_body_util::{BodyExt, Empty, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

pub struct HttpClient {
    full_client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
    empty_client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Empty<Bytes>,
    >,
}

impl HttpClient {
    pub fn new() -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();

        let full_client = Client::builder(TokioExecutor::new()).build(connector.clone());
        let empty_client = Client::builder(TokioExecutor::new()).build(connector);

        Self {
            full_client,
            empty_client,
        }
    }

    pub async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, String> {
        let req = Request::builder()
            .method("POST")
            .uri(url)
            .header("Content-Type", content_type)
            .body(Full::new(Bytes::from(body.to_vec())))
            .map_err(|e| e.to_string())?;

        let response = self
            .full_client
            .request(req)
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(body.to_bytes().to_vec())
    }

    pub async fn get_with_auth(&self, url: &str, token: &str) -> Result<Vec<u8>, String> {
        let req = Request::builder()
            .method("GET")
            .uri(url)
            .header("Authorization", format!("Bearer {}", token))
            .body(Empty::new())
            .map_err(|e| e.to_string())?;

        let response = self
            .empty_client
            .request(req)
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(body.to_bytes().to_vec())
    }

    pub async fn post_with_auth(
        &self,
        url: &str,
        token: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, String> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let user_agent = format!(
            "antigravity/{} {}/{}",
            crate::cloudcode::request::UPSTREAM_VERSION,
            os,
            arch
        );

        let client_metadata = r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#;

        let req = Request::builder()
            .method("POST")
            .uri(url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", content_type)
            .header("User-Agent", user_agent)
            .header(
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header("Client-Metadata", client_metadata)
            .body(Full::new(Bytes::from(body.to_vec())))
            .map_err(|e| e.to_string())?;

        let response = self
            .full_client
            .request(req)
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(body.to_bytes().to_vec())
    }

    pub async fn post_with_headers(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, String> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let user_agent = format!(
            "antigravity/{} {}/{}",
            crate::cloudcode::request::UPSTREAM_VERSION,
            os,
            arch
        );

        let client_metadata = r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#;

        let mut req = Request::builder()
            .method("POST")
            .uri(url)
            .header("Content-Type", content_type)
            .header("User-Agent", user_agent)
            .header(
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header("Client-Metadata", client_metadata);

        for (name, value) in headers {
            req = req.header(*name, *value);
        }

        let req = req
            .body(Full::new(Bytes::from(body.to_vec())))
            .map_err(|e| e.to_string())?;

        let response = self
            .full_client
            .request(req)
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(body.to_bytes().to_vec())
    }

    /// Simple GET request with custom headers
    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>, String> {
        let mut req = Request::builder().method("GET").uri(url);

        for (name, value) in headers {
            req = req.header(*name, *value);
        }

        let req = req.body(Empty::new()).map_err(|e| e.to_string())?;

        let response = self
            .empty_client
            .request(req)
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(body.to_bytes().to_vec())
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}
