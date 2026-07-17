//! HTTP server: read-only `/health` and write-only `/update_firmware`.
//!
//! Both routes are served over plain HTTP on the local network
//! (port 80) by a single picoserve task. Same threat model as the
//! rest of the firmware: the device sits behind the user's wifi, no
//! auth on the wire.
//!
//! Architecture notes:
//!
//! * Runs in its own embassy task so a stalled client can't delay
//!   `watchdog_feeder_task`. picoserve handles request framing,
//!   bounded buffers, and read/write timeouts internally; the watchdog
//!   isolation is *ours* to enforce by giving it a dedicated task.
//! * Pool size = 1 — one in-flight connection at a time. While an OTA
//!   upload is in progress, `/health` requests queue at the TCP accept
//!   layer. This is a debug surface, not a fan-out API.
//! * **`/health`**: no drift surface — the response body *is*
//!   `persist::Snapshot`, serialized via its derived `Serialize` impl
//!   (see `pico-core::persist_data`). Adding a field to `Snapshot`
//!   automatically appears in the JSON. No alloc on this path.
//! * **`/update_firmware`**: streams the request body in 1 KiB chunks
//!   through embassy-boot's `BlockingFirmwareUpdater` (via the `ota`
//!   module), then signals a separate reboot task to fire after the
//!   200 OK response has flushed. On any error mid-stream we write a
//!   500, do *not* call `ota::OtaSession::finalize`, and do not
//!   request a reboot — the next reset boots the existing ACTIVE
//!   image because `mark_updated()` was never called.

#![allow(clippy::needless_pass_by_value)]

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_net::Stack;
use picoserve::{
    io::Read,
    make_static,
    response::{IntoResponse, Json, StatusCode},
    routing::{get, post_service},
    AppBuilder, AppRouter,
};

use crate::{ota, persist};

const PORT: u16 = 80;

/// One connection at a time. While `/update_firmware` is uploading,
/// `/health` queues at the TCP layer — acceptable for a debug
/// surface, and predictable for memory.
const WEB_TASK_POOL_SIZE: usize = 1;

/// Bumped from 1 KiB so OTA uploads can drain network packets faster
/// than embassy-net would force socket-level back-pressure. The
/// `/health` path is request-line + headers only, well under this.
const TCP_RX_BUF_LEN: usize = 4096;
const TCP_TX_BUF_LEN: usize = 1024;

/// picoserve's internal buffer for parsing the request line + headers.
/// A typical browser GET fits well inside 2 KiB.
const HTTP_BUF_LEN: usize = 2048;

/// Per-`read()` chunk size when streaming an OTA upload from the
/// socket into DFU. Smaller than the 4 KiB RP2040 erase sector on
/// purpose: `write_firmware` handles sub-sector writes correctly, and
/// keeping this buffer modest avoids inflating the picoserve task's
/// future state.
const OTA_READ_CHUNK: usize = 1024;

struct AppProps;

impl AppBuilder for AppProps {
    type PathRouter = impl picoserve::routing::PathRouter;

    fn build_app(self) -> picoserve::Router<Self::PathRouter> {
        picoserve::Router::new()
            .route("/health", get(health_handler))
            .route("/update_firmware", post_service(UpdateFirmware))
    }
}

/// `Connection: close` after each response — we never reuse sockets,
/// so keep-alive would just hold resources for nothing. picoserve's
/// default timeouts (5s start / 3s read / 1s write) bound how long a
/// slow or hung client can keep the task busy, which is what protects
/// the watchdog feeder from being indirectly starved.
static CONFIG: picoserve::Config =
    picoserve::Config::const_default().close_connection_after_response();

async fn health_handler() -> impl IntoResponse {
    Json(persist::read_live())
}

/// `POST /update_firmware` handler.
///
/// Implements `RequestHandlerService` directly (rather than using a
/// typed extractor like `Json<T>`) so the body can be streamed in
/// chunks via `embedded_io_async::Read` — required for an image
/// larger than any reasonable buffer.
struct UpdateFirmware;

impl picoserve::routing::RequestHandlerService<()> for UpdateFirmware {
    async fn call_request_handler_service<
        R: Read,
        W: picoserve::response::ResponseWriter<Error = R::Error>,
    >(
        &self,
        _state: &(),
        _path_params: (),
        mut request: picoserve::request::Request<'_, R>,
        response_writer: W,
    ) -> Result<picoserve::ResponseSent, W::Error> {
        let content_length = request.body_connection.content_length();

        // Size sanity-check up front so an oversized POST never
        // touches the updater. `content_length` is `usize` already in
        // picoserve 0.18; comparing against `dfu_partition_size()`
        // (also `usize`).
        let dfu_size = ota::dfu_partition_size();
        if content_length == 0 || content_length > dfu_size {
            warn!(
                "/update_firmware: rejecting content_length={} (DFU={})",
                content_length, dfu_size
            );
            let status = if content_length == 0 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            };
            return (status, "invalid content_length\n")
                .write_to(
                    request.body_connection.finalize().await?,
                    response_writer,
                )
                .await;
        }

        info!(
            "/update_firmware: streaming {} bytes into DFU",
            content_length
        );

        let mut session = ota::OtaSession::begin();
        let mut offset: u32 = 0;
        let mut buf = [0u8; OTA_READ_CHUNK];
        let mut stream_error: Option<&'static str> = None;

        // Inner scope so the reader's mutable borrow of body_connection
        // ends before we call `.finalize()` on it below.
        {
            let mut reader = request.body_connection.body().reader();
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => {
                        warn!(
                            "/update_firmware: body read failed at offset {}",
                            offset
                        );
                        stream_error = Some("body read failed");
                        break;
                    }
                };
                if let Err(e) = session.write_chunk(offset, &buf[..n]) {
                    warn!(
                        "/update_firmware: flash write failed at offset {}: {:?}",
                        offset, e
                    );
                    stream_error = Some("flash write failed");
                    break;
                }
                offset += n as u32;
            }
        }

        if let Some(msg) = stream_error {
            // Do NOT call `session.finalize()` — `mark_updated` was
            // never set, so the bootloader will keep ACTIVE on the
            // next reset. The half-written DFU bank is discarded on
            // the next legitimate upload (write_firmware re-erases as
            // it goes).
            return (StatusCode::INTERNAL_SERVER_ERROR, msg)
                .write_to(
                    request.body_connection.finalize().await?,
                    response_writer,
                )
                .await;
        }

        if offset as usize != content_length {
            warn!(
                "/update_firmware: short body (got {} of {})",
                offset, content_length
            );
            return (StatusCode::BAD_REQUEST, "short body\n")
                .write_to(
                    request.body_connection.finalize().await?,
                    response_writer,
                )
                .await;
        }

        if let Err(e) = session.finalize() {
            warn!("/update_firmware: mark_updated failed: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "finalize failed\n")
                .write_to(
                    request.body_connection.finalize().await?,
                    response_writer,
                )
                .await;
        }

        info!(
            "/update_firmware: {} bytes staged into DFU, rebooting",
            offset
        );

        // Write the success response, then schedule a reboot. The
        // dedicated `ota::reboot_task` waits ~500 ms after the
        // signal so TCP has time to flush the response onto the wire
        // (and the operator's `curl` sees `200 OK`) before reset.
        // Body is JSON but served as text/plain since picoserve's
        // simple tuple-response API doesn't carry a content type;
        // operators using `jq` on the response work fine either way.
        let sent = (StatusCode::OK, "{\"status\":\"update_staged\"}\n")
            .write_to(
                request.body_connection.finalize().await?,
                response_writer,
            )
            .await?;
        ota::request_reboot();
        Ok(sent)
    }
}

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(task_id: usize, stack: Stack<'static>, app: &'static AppRouter<AppProps>) -> ! {
    let mut tcp_rx_buffer = [0u8; TCP_RX_BUF_LEN];
    let mut tcp_tx_buffer = [0u8; TCP_TX_BUF_LEN];
    let mut http_buffer = [0u8; HTTP_BUF_LEN];

    picoserve::Server::new(app, &CONFIG, &mut http_buffer)
        .listen_and_serve(task_id, stack, PORT, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}

/// Spawn the HTTP server. Call once after the embassy-net stack has
/// a DHCP lease.
pub fn spawn(spawner: Spawner, stack: Stack<'static>) {
    let app = make_static!(AppRouter<AppProps>, AppProps.build_app());
    for task_id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(task_id, stack, app).unwrap());
    }
}
