//! Read-only `/health` endpoint served over plain HTTP on the local
//! network. Returns the live persist-region snapshot as JSON so the
//! same data exposed by the bin's `dump-persist` debug-port tool is
//! accessible from any device on the LAN with `curl`.
//!
//! Architecture notes:
//!
//! * Runs in its own embassy task so a stalled client can't delay
//!   `watchdog_feeder_task`. picoserve handles request framing,
//!   bounded buffers, and read/write timeouts internally; the watchdog
//!   isolation is *ours* to enforce by giving it a dedicated task.
//! * Pool size = 1 — one in-flight connection at a time. This is a
//!   debug endpoint, not a multi-tenant API. Concurrent clients queue
//!   at the TCP accept layer.
//! * Plain HTTP, no auth. Same threat model as the rest of the
//!   firmware: it sits on the local network behind the user's wifi.
//! * **No drift surface.** The response body *is* `persist::Snapshot`,
//!   serialized via its derived `Serialize` impl (see
//!   `pico-core::persist_data`). Adding a field to `Snapshot`
//!   automatically appears in `/health` — there is no parallel
//!   response struct that can fall behind. The host-side round-trip
//!   test in `persist_data::tests` locks in the JSON shape.
//! * No alloc on this path. picoserve's `json` feature serializes the
//!   struct via `serde-json-core` directly to the socket, so `/health`
//!   keeps working under heap pressure (which is exactly when it's
//!   most useful for debugging).

#![allow(clippy::needless_pass_by_value)]

use embassy_executor::Spawner;
use embassy_net::Stack;
use picoserve::{
    make_static,
    response::{IntoResponse, Json},
    routing::get,
    AppBuilder, AppRouter,
};

use crate::persist;

const PORT: u16 = 80;

/// One connection at a time. The endpoint is for interactive debug use
/// (one `curl` from a laptop), not a fan-out API; serializing keeps
/// memory predictable.
const WEB_TASK_POOL_SIZE: usize = 1;

const TCP_RX_BUF_LEN: usize = 1024;
const TCP_TX_BUF_LEN: usize = 1024;
/// picoserve's internal buffer for parsing the request line + headers.
/// A typical browser GET fits well inside 2 KiB.
const HTTP_BUF_LEN: usize = 2048;

struct AppProps;

impl AppBuilder for AppProps {
    type PathRouter = impl picoserve::routing::PathRouter;

    fn build_app(self) -> picoserve::Router<Self::PathRouter> {
        picoserve::Router::new().route("/health", get(health_handler))
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

/// Spawn the `/health` server. Call once after the embassy-net stack
/// has a DHCP lease.
pub fn spawn(spawner: Spawner, stack: Stack<'static>) {
    let app = make_static!(AppRouter<AppProps>, AppProps.build_app());
    for task_id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(task_id, stack, app).unwrap());
    }
}
