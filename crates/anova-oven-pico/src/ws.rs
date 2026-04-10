//! WebSocket-over-TLS transport for the Anova oven control protocol.
//!
//! Uses `embedded-tls` for TLS 1.3 and `websocketz` for the WebSocket layer,
//! both over an `embassy-net` TCP socket.

extern crate alloc;

use alloc::format;
use core::net::Ipv4Addr;

use defmt::{info, warn};
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::Duration;
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, UnsecureProvider};
use rand::rngs::SmallRng;
use rand::SeedableRng;
use websocketz::http::Header;
use websocketz::options::ConnectOptions;
use websocketz::WebSocket;

use crate::rng::Rng06;

const ANOVA_HOST: &str = "devices.anovaculinary.io";
const ANOVA_PORT: u16 = 443;

/// Connect to the Anova WebSocket API over TLS and run the message loop.
///
/// This function:
/// 1. Resolves the Anova API hostname via DNS
/// 2. Opens a TCP connection to port 443
/// 3. Performs a TLS 1.3 handshake
/// 4. Upgrades to WebSocket with the `ANOVA_V2` subprotocol
/// 5. Reads incoming events and passes them to `anova_oven_protocol::parse_message`
pub async fn run_websocket(
    stack: Stack<'static>,
    token: &str,
    tls_read: &mut [u8],
    tls_write: &mut [u8],
) {
    // DNS resolve.
    info!("Resolving {}...", ANOVA_HOST);
    let host_addr = match stack
        .dns_query(ANOVA_HOST, embassy_net::dns::DnsQueryType::A)
        .await
    {
        Ok(addrs) if !addrs.is_empty() => match addrs[0] {
            embassy_net::IpAddress::Ipv4(a) => {
                let octets = a.octets();
                let addr = Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]);
                info!("Resolved to {}", addr);
                addr
            }
        },
        Ok(_) => {
            warn!("DNS returned no addresses");
            return;
        }
        Err(e) => {
            warn!("DNS query failed: {}", e);
            return;
        }
    };

    // TCP connect.
    let mut rx_buf = [0u8; 4096];
    let mut tx_buf = [0u8; 4096];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(30)));

    info!("TCP connecting to {}:{}...", host_addr, ANOVA_PORT);
    if let Err(e) = socket.connect((host_addr, ANOVA_PORT)).await {
        warn!("TCP connect failed: {}", e);
        return;
    }
    info!("TCP connected");

    // TLS handshake.
    let mut tls: TlsConnection<_, Aes128GcmSha256> =
        TlsConnection::new(socket, tls_read, tls_write);

    let mut rng = Rng06::new(0xfedcba98_76543210);
    let tls_config = TlsConfig::new().with_server_name(ANOVA_HOST);
    if let Err(e) = tls
        .open(TlsContext::new(
            &tls_config,
            UnsecureProvider::new::<Aes128GcmSha256>(&mut rng),
        ))
        .await
    {
        warn!("TLS handshake failed: {}", defmt::Debug2Format(&e));
        return;
    }
    info!("TLS connected");

    // WebSocket upgrade.
    let path = format!(
        "/?token={}&supportedAccessories=APO&platform=android",
        token,
    );
    let headers = [Header {
        name: "Sec-WebSocket-Protocol",
        value: b"ANOVA_V2",
    }];
    let opts = match ConnectOptions::default().with_path(&path) {
        Ok(o) => o.with_headers(&headers),
        Err(_) => {
            warn!("Invalid WebSocket path");
            return;
        }
    };

    let mut ws_read_buf = [0u8; 4096];
    let mut ws_write_buf = [0u8; 1024];
    let mut ws_frag_buf = [0u8; 1024];
    let mut ws_rng = SmallRng::seed_from_u64(0xdeadbeef_cafebabe);

    let mut ws = match WebSocket::connect::<16>(
        opts,
        tls,
        &mut ws_rng,
        &mut ws_read_buf,
        &mut ws_write_buf,
        &mut ws_frag_buf,
    )
    .await
    {
        Ok(ws) => ws,
        Err(_) => {
            warn!("WebSocket upgrade failed");
            return;
        }
    };
    info!("WebSocket connected to Anova API");

    // Message loop.
    loop {
        match websocketz::next!(ws) {
            Some(Ok(msg)) => {
                match msg {
                    websocketz::Message::Text(text) => {
                        match anova_oven_protocol::parse_message(text.as_bytes()) {
                            Ok(anova_oven_protocol::Event::ApoState(state)) => {
                                info!(
                                    "Oven state: cooker={} mode={}",
                                    state.cooker_id.as_str(),
                                    state.state.state.mode.as_str(),
                                );
                            }
                            Ok(anova_oven_protocol::Event::ApoWifiList { cooker_id }) => {
                                if let Some(id) = &cooker_id {
                                    info!("WiFi list, cooker_id={}", id.as_str());
                                }
                            }
                            Ok(anova_oven_protocol::Event::Response {
                                request_id,
                                status,
                            }) => {
                                info!(
                                    "Response: req={} status={}",
                                    request_id.as_str(),
                                    status.as_str()
                                );
                            }
                            Ok(_) => {
                                info!("Other event received");
                            }
                            Err(_) => {
                                warn!("Failed to parse message");
                            }
                        }
                    }
                    websocketz::Message::Binary(_) => {
                        info!("Binary message received (unexpected)");
                    }
                    websocketz::Message::Close(_) => {
                        info!("Server closed connection");
                        break;
                    }
                    _ => {}
                }
            }
            Some(Err(_)) => {
                warn!("WebSocket read error");
                break;
            }
            None => {
                info!("WebSocket stream ended");
                break;
            }
        }
    }
}
