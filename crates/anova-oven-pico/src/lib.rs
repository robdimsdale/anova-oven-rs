#![no_std]
#![allow(async_fn_in_trait)]

//! Embedded integration for the firestore recipe fetch.
//!
//! The Pico W doesn't yet have the TLS/HTTPS layer needed to actually send
//! these requests, but this module demonstrates the intended integration
//! pattern using the `anova-oven-firestore` crate:
//!
//! 1. Build a query with one of the `queries::` helpers.
//! 2. POST the JSON body to the REST URL, with the Firebase ID token as a
//!    bearer credential.
//! 3. Parse the response via `serde_json::from_slice::<RunQueryResponse>`.
//! 4. Unwrap each Firestore document into a concrete
//!    [`anova_oven_firestore::OvenRecipe`] via `OvenRecipe::from_document`.
//!
//! Provide any HTTP transport that implements [`HttpClient`] (e.g. an
//! `embassy-net` TCP socket wrapped in `embedded-tls`). This crate stays
//! transport-agnostic.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use anova_oven_firestore::{
    auth::{self, SignInRequest, SignInResponse},
    firestore::{run_query_url, RunQueryResponse},
    queries, ANOVA_OVEN_API_KEY, ANOVA_PROJECT_ID,
};

/// Minimal HTTP client contract needed by the recipe fetch flow.
///
/// An implementation would wrap an `embassy-net` TCP socket plus TLS and
/// issue HTTPS POST and GET requests. Keeping this trait in the consuming
/// crate (rather than in `anova-oven-firestore`) avoids pulling any
/// embedded networking types into the shared protocol crate.
pub trait HttpClient {
    type Error;

    /// POST a JSON body, returning the response body bytes on success.
    async fn post_json(
        &mut self,
        url: &str,
        body: &[u8],
        bearer_token: Option<&str>,
    ) -> Result<Vec<u8>, Self::Error>;

    /// POST form-encoded data (for the Secure Token Service refresh endpoint).
    async fn post_form(
        &mut self,
        url: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, Self::Error>;
}

#[derive(Debug)]
pub enum FetchError<E> {
    Http(E),
    Serde(serde_json::Error),
    SignIn,
}

impl<E> From<serde_json::Error> for FetchError<E> {
    fn from(e: serde_json::Error) -> Self {
        FetchError::Serde(e)
    }
}

/// Sign in with email and password. Returns the ID token and UID.
pub async fn sign_in<C: HttpClient>(
    client: &mut C,
    email: &str,
    password: &str,
) -> Result<(String, String), FetchError<C::Error>> {
    let url = auth::sign_in_url(ANOVA_OVEN_API_KEY);
    let body = serde_json::to_vec(&SignInRequest::new(email, password))?;
    let bytes = client
        .post_json(&url, &body, None)
        .await
        .map_err(FetchError::Http)?;
    let parsed: SignInResponse = serde_json::from_slice(&bytes)?;
    Ok((parsed.id_token, parsed.local_id))
}

/// Fetch the user's non-draft recipes. Returns deserialized
/// [`anova_oven_firestore::OvenRecipe`] values, ready for use with
/// `CMD_APO_START`.
pub async fn fetch_user_recipes<C: HttpClient>(
    client: &mut C,
    id_token: &str,
    uid: &str,
    limit: u32,
) -> Result<Vec<anova_oven_firestore::OvenRecipe>, FetchError<C::Error>> {
    let q = queries::user_recipes(ANOVA_PROJECT_ID, uid, limit);
    let url = run_query_url(ANOVA_PROJECT_ID, &q.parent_path);
    let body = serde_json::to_vec(&q.body)?;
    let bytes = client
        .post_json(&url, &body, Some(id_token))
        .await
        .map_err(FetchError::Http)?;
    let items: RunQueryResponse = serde_json::from_slice(&bytes)?;

    let mut out = Vec::new();
    for item in items {
        if let Some(doc) = item.document {
            out.push(anova_oven_firestore::OvenRecipe::from_document(&doc)?);
        }
    }
    Ok(out)
}
