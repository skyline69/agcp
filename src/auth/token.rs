use serde::Deserialize;

use crate::error::{AuthError, Error, Result};

// These OAuth client credentials are **intentionally public**. AGCP uses Google's
// "installed application" (native/CLI) OAuth flow, where the client secret cannot
// be kept confidential. Google documents this explicitly:
// https://developers.google.com/identity/protocols/oauth2/native-app
//
// The security of this flow relies on the PKCE challenge and the redirect to
// localhost, NOT on the client secret. Extracting these values grants no access
// to any user's data — a valid refresh token is still required.
//
// See also: SECURITY.md § "What Does NOT Count"
pub const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const CLIENT_SECRET: &str = "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";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo";

pub async fn refresh_access_token(
    http_client: &super::HttpClient,
    refresh_token: &str,
) -> Result<(String, u64)> {
    // Handle composite format: refreshToken|projectId|managedProjectId
    let actual_refresh_token = refresh_token.split('|').next().unwrap_or(refresh_token);

    let body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        CLIENT_ID, CLIENT_SECRET, actual_refresh_token
    );

    let response = http_client
        .post(
            TOKEN_URL,
            "application/x-www-form-urlencoded",
            body.as_bytes(),
        )
        .await
        .map_err(|e| Error::Auth(AuthError::RefreshFailed(e.to_string())))?;

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        expires_in: u64,
    }

    let tokens: TokenResponse = serde_json::from_slice(&response)
        .map_err(|e| Error::Auth(AuthError::RefreshFailed(e.to_string())))?;

    Ok((tokens.access_token, tokens.expires_in))
}

pub async fn get_user_email(http_client: &super::HttpClient, access_token: &str) -> Result<String> {
    let response = http_client
        .get_with_auth(USERINFO_URL, access_token)
        .await
        .map_err(|e| Error::Auth(AuthError::OAuthFailed(e.to_string())))?;

    #[derive(Deserialize)]
    struct UserInfo {
        email: String,
    }

    let user_info: UserInfo = serde_json::from_slice(&response)
        .map_err(|e| Error::Auth(AuthError::OAuthFailed(e.to_string())))?;

    Ok(user_info.email)
}
