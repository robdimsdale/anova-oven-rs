use defmt::{debug, info, warn};
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use reqwless::client::HttpClient;
use reqwless::headers::ContentType;
use reqwless::request::{Method, RequestBuilder};

// cyw43's SPI/DMA layer requires 4-byte aligned buffers (asserts addr % 4 == 0).
// [u8; N] only guarantees 1-byte alignment, so we use a repr(align(4)) wrapper.
#[repr(align(4))]
pub(crate) struct Aligned<const N: usize>(pub(crate) [u8; N]);

pub(crate) const HTTP_RX_BUF_LEN: usize = 16384;

/// Typed failure cause for an API call. Keeps the five distinct failure points
/// distinguishable (review §6.1) instead of collapsing them all into `None` /
/// `Err(())`. The underlying error detail (reqwless / serde_json) is logged at
/// the failure site with `defmt::Debug2Format` before being converted into one
/// of these variants, so log volume is unchanged but callers can branch on
/// kind if they want to.
#[derive(Debug, defmt::Format)]
pub enum ApiError {
    /// TCP connect / DNS / TLS handshake failed before the request was sent.
    Connect,
    /// Sending the request headers/body failed mid-flight.
    Send,
    /// Reading the response body into the RX buffer failed.
    BodyRead,
    /// Server responded with a non-success status (or one outside what the
    /// caller accepts; e.g. `fetch_status` rejects everything but 200).
    Http(u16),
    /// Response body did not deserialize.
    Json,
}

/// Normalizes the configured server URL (trim trailing `/`, default to
/// `http://`). The result never changes, so the caller computes it once at
/// startup and threads `&str` in rather than re-allocating per request
/// (review §2.1).
pub(crate) fn normalize_server_url(url: &str) -> alloc::string::String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.into()
    } else {
        alloc::format!("http://{trimmed}")
    }
}

/// Shared transport for every API call (review §3.2). Does the boilerplate —
/// build client / open request / optionally attach JSON body / send / read
/// body — and hands the response off to `handler` *while the response is
/// still in scope*, since the response body slice borrows from the request
/// machinery and can't escape this function.
///
/// `TX`/`RX` are the per-socket buffer sizes inside `TcpClientState`; small
/// endpoints use 1024/1024, the recipe list (larger response) uses 4096/4096.
/// `label` is a short tag like "GET /status" used purely for log messages so
/// errors stay identifiable across the shared helper.
#[allow(clippy::too_many_arguments)] // private helper; a struct/builder is more ceremony than this needs
async fn request<R, F, const TX: usize, const RX: usize>(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    server: &str,
    method: Method,
    path: &str,
    label: &str,
    json_body: Option<&[u8]>,
    handler: F,
) -> Result<R, ApiError>
where
    F: FnOnce(u16, &[u8]) -> Result<R, ApiError>,
{
    let client_state = TcpClientState::<1, TX, RX>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let url = alloc::format!("{server}{path}");
    let req_init = match client.request(method, &url).await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "{}: connection failed: {}",
                label,
                defmt::Debug2Format(&e)
            );
            return Err(ApiError::Connect);
        }
    };

    // reqwless' `body()` + `content_type()` change the request's typestate
    // (the body type is a type parameter), so the with-body and no-body
    // branches end up with different concrete `HttpRequestHandle<…>` types.
    // The response borrows back into the request, so both `req` and the
    // send/read/handle steps that follow must live in the same scope. We
    // therefore inline the send/read/handle into each branch and `return`
    // out of it directly — `handler` is `FnOnce` but only one branch runs,
    // so the borrow checker is happy.
    if let Some(body) = json_body {
        let mut req = req_init
            .body(body)
            .content_type(ContentType::ApplicationJson);
        let response = match req.send(rx_buf).await {
            Ok(r) => r,
            Err(e) => {
                warn!("{}: send failed: {}", label, defmt::Debug2Format(&e));
                return Err(ApiError::Send);
            }
        };
        let status = response.status.0;
        let body_slice = match response.body().read_to_end().await {
            Ok(b) => b,
            Err(_) => {
                warn!("{}: failed to read body", label);
                return Err(ApiError::BodyRead);
            }
        };
        handler(status, body_slice)
    } else {
        let mut req = req_init;
        let response = match req.send(rx_buf).await {
            Ok(r) => r,
            Err(e) => {
                warn!("{}: send failed: {}", label, defmt::Debug2Format(&e));
                return Err(ApiError::Send);
            }
        };
        let status = response.status.0;
        let body_slice = match response.body().read_to_end().await {
            Ok(b) => b,
            Err(_) => {
                warn!("{}: failed to read body", label);
                return Err(ApiError::BodyRead);
            }
        };
        handler(status, body_slice)
    }
}

pub async fn fetch_status(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    server: &str,
) -> Result<anova_oven_api::OvenStatus, ApiError> {
    debug!(
        "GET /status: link_up={} config_up={}",
        stack.is_link_up(),
        stack.is_config_up()
    );
    request::<_, _, 1024, 1024>(
        stack,
        rx_buf,
        server,
        Method::GET,
        "/status",
        "GET /status",
        None,
        |status, body| {
            if status != 200 {
                warn!("GET /status: HTTP {}", status);
                return Err(ApiError::Http(status));
            }
            match serde_json::from_slice::<anova_oven_api::OvenStatus>(body) {
                Ok(s) => {
                    debug!(
                        "Status: mode={} temp={}F target={}F steam={}% door={} water={}",
                        s.mode.as_str(),
                        celcius_to_fahrenheit(s.current_temperature_c()),
                        celcius_to_fahrenheit(s.target_temperature_c.unwrap_or(0.0)),
                        s.steam_pct,
                        s.door_open,
                        s.water_tank_empty,
                    );
                    Ok(s)
                }
                Err(e) => {
                    warn!(
                        "GET /status: failed to parse JSON: {}",
                        defmt::Debug2Format(&e)
                    );
                    Err(ApiError::Json)
                }
            }
        },
    )
    .await
}

/// Polls `/current-cook`. The two outcome axes stay orthogonal (review §1.8):
/// `Ok(Some)` = a cook is in progress, `Ok(None)` = HTTP 204 / no cook (still
/// a successful poll), `Err(_)` = transport/HTTP/parse failure with a typed
/// cause. Only `Ok(_)` counts as a successful poll on the caller side.
pub async fn fetch_current_cook(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    server: &str,
) -> Result<Option<anova_oven_api::CurrentCook>, ApiError> {
    request::<_, _, 1024, 1024>(
        stack,
        rx_buf,
        server,
        Method::GET,
        "/current-cook",
        "GET /current-cook",
        None,
        |status, body| match status {
            204 => Ok(None),
            200 => match serde_json::from_slice::<anova_oven_api::CurrentCook>(body) {
                Ok(cook) => {
                    #[cfg(feature = "verbose-logs")]
                    info!(
                        "Current cook: {} ({} stages)",
                        cook.recipe_title.as_str(),
                        cook.total_stage_count,
                    );
                    Ok(Some(cook))
                }
                Err(e) => {
                    warn!(
                        "GET /current-cook: failed to parse JSON: {}",
                        defmt::Debug2Format(&e)
                    );
                    Err(ApiError::Json)
                }
            },
            other => {
                warn!("GET /current-cook: HTTP {}", other);
                Err(ApiError::Http(other))
            }
        },
    )
    .await
}

pub async fn send_stop(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    server: &str,
) -> Result<(), ApiError> {
    request::<_, _, 1024, 1024>(
        stack,
        rx_buf,
        server,
        Method::POST,
        "/stop",
        "POST /stop",
        None,
        |status, _body| {
            if (200..300).contains(&status) {
                info!("POST /stop: success (HTTP {})", status);
                Ok(())
            } else {
                warn!("POST /stop: HTTP {}", status);
                Err(ApiError::Http(status))
            }
        },
    )
    .await
}

pub async fn send_start(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    server: &str,
    recipe_id: &str,
) -> Result<(), ApiError> {
    // Build JSON body: {"recipe_id": "..."}. Owned by this stack frame so the
    // helper's `Some(&[u8])` borrow stays valid for the whole call.
    let body = alloc::format!(r#"{{"recipe_id":"{}"}}"#, recipe_id);
    request::<_, _, 1024, 1024>(
        stack,
        rx_buf,
        server,
        Method::POST,
        "/start",
        "POST /start",
        Some(body.as_bytes()),
        |status, _body| {
            if (200..300).contains(&status) {
                info!("POST /start: success (HTTP {})", status);
                Ok(())
            } else {
                warn!("POST /start: HTTP {}", status);
                Err(ApiError::Http(status))
            }
        },
    )
    .await
}

pub async fn fetch_recipes(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    server: &str,
) -> Result<alloc::vec::Vec<anova_oven_api::Recipe>, ApiError> {
    request::<_, _, 4096, 4096>(
        stack,
        rx_buf,
        server,
        Method::GET,
        "/recipes",
        "GET /recipes",
        None,
        |status, body| {
            if status != 200 {
                warn!("GET /recipes: HTTP {}", status);
                return Err(ApiError::Http(status));
            }
            match serde_json::from_slice::<alloc::vec::Vec<anova_oven_api::Recipe>>(body) {
                Ok(mut recipes) => {
                    // Normalize all recipes for Anova compatibility
                    for recipe in &mut recipes {
                        recipe.normalize();
                    }
                    #[cfg(feature = "verbose-logs")]
                    {
                        info!("Recipes: {} found", recipes.len());
                        for recipe in &recipes {
                            info!(
                                "  - {} ({} stages)",
                                recipe.title.as_str(),
                                recipe.stage_count
                            );
                        }
                    }
                    Ok(recipes)
                }
                Err(e) => {
                    warn!(
                        "GET /recipes: failed to parse JSON: {}",
                        defmt::Debug2Format(&e)
                    );
                    Err(ApiError::Json)
                }
            }
        },
    )
    .await
}

pub fn celcius_to_fahrenheit(c: f32) -> f32 {
    c * 1.8 + 32.0
}
