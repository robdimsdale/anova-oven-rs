//! Firestore HTTP transport using `reqwless` + `embedded-tls` over `embassy-net`.
//!
//! Implements the [`HttpClient`](anova_oven_pico::HttpClient) trait from `lib.rs`
//! so that [`anova_oven_pico::sign_in`] and [`anova_oven_pico::fetch_user_recipes`]
//! work on the Pico W.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use defmt::{info, warn};
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::Stack;
use reqwless::client::{HttpClient as ReqwlessHttpClient, TlsConfig, TlsVerify};
use reqwless::request::{Method, RequestBuilder as _};

use anova_oven_pico::HttpClient;

/// Error type for the reqwless-based HTTP client.
#[derive(Debug)]
pub enum HttpError {
    Reqwless,
}

/// Wrapper around reqwless that implements our [`HttpClient`] trait.
pub struct PicoHttpClient<'a> {
    http: ReqwlessHttpClient<'a, TcpClient<'a, 1, 4096, 4096>, DnsSocket<'a>>,
}

impl<'a> PicoHttpClient<'a> {
    pub fn new(
        tcp: &'a TcpClient<'a, 1, 4096, 4096>,
        dns: &'a DnsSocket<'a>,
        tls_read: &'a mut [u8],
        tls_write: &'a mut [u8],
        seed: u64,
    ) -> Self {
        let tls = TlsConfig::new(seed, tls_read, tls_write, TlsVerify::None);
        Self {
            http: ReqwlessHttpClient::new_with_tls(tcp, dns, tls),
        }
    }

    /// Internal helper: POST with headers and body, return response bytes.
    async fn do_post(
        &mut self,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        let mut rx_buf = [0u8; 8192];

        let handle = self
            .http
            .request(Method::POST, url)
            .await
            .map_err(|_| HttpError::Reqwless)?;

        let mut handle = handle.headers(headers).body(body);

        let response = handle
            .send(&mut rx_buf)
            .await
            .map_err(|_| HttpError::Reqwless)?;

        let status = response.status.0;
        if status < 200 || status >= 300 {
            warn!("HTTP POST -> status {}", status);
        }

        let resp_body = response
            .body()
            .read_to_end()
            .await
            .map_err(|_| HttpError::Reqwless)?;

        Ok(resp_body.to_vec())
    }
}

impl HttpClient for PicoHttpClient<'_> {
    type Error = HttpError;

    async fn post_json(
        &mut self,
        url: &str,
        body: &[u8],
        bearer_token: Option<&str>,
    ) -> Result<Vec<u8>, Self::Error> {
        let mut auth_value = String::new();
        if let Some(token) = bearer_token {
            auth_value.push_str("Bearer ");
            auth_value.push_str(token);
        }

        if bearer_token.is_some() {
            let headers: [(&str, &str); 2] = [
                ("Content-Type", "application/json"),
                ("Authorization", &auth_value),
            ];
            self.do_post(url, body, &headers).await
        } else {
            let headers: [(&str, &str); 1] = [("Content-Type", "application/json")];
            self.do_post(url, body, &headers).await
        }
    }

    async fn post_form(
        &mut self,
        url: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, Self::Error> {
        let headers: [(&str, &str); 1] =
            [("Content-Type", "application/x-www-form-urlencoded")];
        self.do_post(url, body, &headers).await
    }
}

/// Run the full Firestore flow: sign in, then fetch user recipes.
pub async fn run_firestore_flow(
    stack: Stack<'static>,
    tls_read: &mut [u8],
    tls_write: &mut [u8],
    email: &str,
    password: &str,
) {
    let client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);

    let seed: u64 = 0x0123_4567_89ab_cdef;
    let mut client = PicoHttpClient::new(&tcp, &dns, tls_read, tls_write, seed);

    info!("Signing in...");
    match anova_oven_pico::sign_in(&mut client, email, password).await {
        Ok((id_token, uid)) => {
            info!("Signed in, uid = {}", uid.as_str());

            info!("Fetching recipes...");
            match anova_oven_pico::fetch_user_recipes(&mut client, &id_token, &uid, 10).await {
                Ok(recipes) => {
                    info!("Fetched {} recipes", recipes.len());
                    for r in &recipes {
                        info!("  - {}", r.title.as_str());
                    }
                }
                Err(_e) => {
                    warn!("Failed to fetch recipes");
                }
            }
        }
        Err(_e) => {
            warn!("Sign-in failed");
        }
    }
}
