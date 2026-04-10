#![no_std]
#![no_main]

extern crate alloc;

use embedded_alloc::LlffHeap as Heap;

#[global_allocator]
static HEAP: Heap = Heap::empty();

use core::net::Ipv4Addr;

use cyw43_pio::PioSpi;
use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// WiFi credentials — in a real deployment these would come from flash/config.
// For now, hardcoded for development.
const WIFI_SSID: &str = "YOUR_WIFI_SSID";
const WIFI_PASSWORD: &str = "YOUR_WIFI_PASSWORD";

// Anova API token — in a real deployment, read from flash.
#[allow(dead_code)]
const ANOVA_TOKEN: &str = "YOUR_ANOVA_TOKEN";

const ANOVA_HOST: &str = "devices.anovaculinary.io";
const ANOVA_PORT: u16 = 443;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

// CYW43 firmware blobs, included at compile time.
// firmware and nvram must be 4-byte aligned for the cyw43 driver.
static FW: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../firmware/43439A0.bin"));
static NVRAM: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../nvram_rp2040.bin"));
// CLM blob is loaded via control.init() (plain bytes, no alignment needed).
static CLM: &[u8] = include_bytes!("../firmware/43439A0_clm.bin");

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // Initialize the heap allocator (needed by serde_json / alloc).
    {
        use core::mem::MaybeUninit;
        const HEAP_SIZE: usize = 16384;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        #[allow(static_mut_refs)]
        unsafe {
            HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE);
        }
    }

    let p = embassy_rp::init(Default::default());

    // Set up CYW43 WiFi chip via PIO SPI.
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let dma = embassy_rp::dma::Channel::new(p.DMA_CH0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        cyw43_pio::DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, FW, NVRAM).await;
    spawner.spawn(cyw43_task(runner).unwrap());

    control.init(CLM).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // Set up network stack.
    let config = Config::dhcpv4(Default::default());

    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    // Fixed seed is fine for a single-device dev setup. A production build
    // would read a hardware RNG or unique chip ID here.
    let seed: u64 = 0x0123_4567_89ab_cdef;
    let (stack, runner) =
        embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);
    spawner.spawn(net_task(runner).unwrap());

    // Connect to WiFi.
    info!("Connecting to WiFi: {}", WIFI_SSID);
    loop {
        match control
            .join(WIFI_SSID, cyw43::JoinOptions::new(WIFI_PASSWORD.as_bytes()))
            .await
        {
            Ok(_) => {
                info!("WiFi connected");
                break;
            }
            Err(e) => {
                warn!("WiFi join failed: {}", e);
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    }

    // Wait for DHCP.
    info!("Waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("Network is up");

    if let Some(config) = stack.config_v4() {
        info!("IP address: {}", config.address);
    }

    // Resolve the Anova API host.
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
            defmt::panic!("DNS returned no addresses");
        }
        Err(e) => {
            defmt::panic!("DNS query failed: {}", e);
        }
    };

    // Establish a TCP connection to the Anova API.
    // NOTE: The Anova API requires TLS (wss://). A full implementation needs
    // an embedded TLS library (e.g. embedded-tls) layered on top of this TCP
    // socket, plus a WebSocket protocol handler. This is the scaffolding for
    // that — we establish the TCP connection to prove network connectivity.
    let mut rx_buf = [0u8; 4096];
    let mut tx_buf = [0u8; 4096];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(30)));

    info!("Connecting to {}:{}...", host_addr, ANOVA_PORT);
    match socket.connect((host_addr, ANOVA_PORT)).await {
        Ok(()) => info!("TCP connected to Anova API"),
        Err(e) => defmt::panic!("TCP connection failed: {}", e),
    }

    // TODO: Layer TLS on the TCP socket using embedded-tls.
    // TODO: Perform WebSocket upgrade handshake over TLS.
    // TODO: Send the token and supportedAccessories query in the upgrade request.
    // TODO: Enter message loop using anova_oven_protocol::parse_message().
    //
    // The protocol crate (anova-oven-protocol) is already no_std compatible
    // and can parse messages once we have raw WebSocket frame payloads:
    //
    //   match anova_oven_protocol::parse_message(&ws_payload) {
    //       Ok(Event::ApoState(state)) => { /* use state */ }
    //       ...
    //   }

    info!("TCP connection established — TLS + WebSocket not yet implemented");
    info!("This proves: WiFi, DHCP, DNS, and TCP connectivity all work");

    // Keep the connection alive so we don't exit.
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
