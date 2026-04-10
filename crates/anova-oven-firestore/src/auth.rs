//! Firebase Identity Toolkit + Secure Token Service request/response types.

use alloc::format;
use alloc::string::String;
use serde::{Deserialize, Serialize};

/// Build the Identity Toolkit URL for email+password sign-in.
///
/// POST this URL with an [`SignInRequest`] JSON body to obtain an
/// [`SignInResponse`] containing the ID token used for Firestore access.
pub fn sign_in_url(api_key: &str) -> String {
    format!(
        "https://identitytoolkit.googleapis.com/v1/accounts:signInWithPassword?key={api_key}"
    )
}

/// Build the Secure Token Service URL for refreshing an ID token.
///
/// POST this URL with a form-encoded body (see [`build_refresh_form`]).
pub fn refresh_token_url(api_key: &str) -> String {
    format!("https://securetoken.googleapis.com/v1/token?key={api_key}")
}

/// Build the form-encoded body for exchanging a refresh token.
///
/// The resulting body should be POSTed with
/// `Content-Type: application/x-www-form-urlencoded`.
pub fn build_refresh_form(refresh_token: &str) -> String {
    // The only values we encode are `grant_type` (fixed) and the refresh token.
    // Firebase refresh tokens are base64url-safe so they do not require
    // percent-encoding in practice, but we escape defensively anyway.
    let mut encoded = String::with_capacity(refresh_token.len() + 48);
    encoded.push_str("grant_type=refresh_token&refresh_token=");
    for b in refresh_token.as_bytes() {
        let c = *b;
        let safe = c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~');
        if safe {
            encoded.push(c as char);
        } else {
            use core::fmt::Write;
            let _ = write!(encoded, "%{:02X}", c);
        }
    }
    encoded
}

#[derive(Debug, Serialize)]
pub struct SignInRequest<'a> {
    pub email: &'a str,
    pub password: &'a str,
    /// Firebase requires this to be `true`.
    #[serde(rename = "returnSecureToken")]
    pub return_secure_token: bool,
}

impl<'a> SignInRequest<'a> {
    pub fn new(email: &'a str, password: &'a str) -> Self {
        Self {
            email,
            password,
            return_secure_token: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInResponse {
    /// Firebase ID token (JWT) — used as `Authorization: Bearer <token>` on
    /// Firestore REST calls.
    pub id_token: String,
    /// Firebase UID — used to construct `user-profiles/{uid}` references.
    pub local_id: String,
    /// Long-lived refresh token for obtaining future ID tokens.
    pub refresh_token: String,
    /// Lifetime of `id_token` in seconds, as a string (Firebase quirk).
    pub expires_in: String,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RefreshTokenResponse {
    /// Fresh ID token.
    pub id_token: String,
    /// New refresh token (may be identical to the one sent).
    pub refresh_token: String,
    /// Lifetime of `id_token` in seconds, as a string.
    pub expires_in: String,
    pub user_id: String,
}
