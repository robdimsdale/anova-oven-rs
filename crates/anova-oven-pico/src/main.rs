#![no_std]
#![no_main]

extern crate alloc;

use embedded_alloc::LlffHeap as Heap;

#[global_allocator]
static HEAP: Heap = Heap::empty();

use cyw43_pio::PioSpi;
use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// Credentials and server address are injected at compile time from environment
// variables. Set them before building:
//
//   export ANOVA_WIFI_SSID="MyNetwork"
//   export ANOVA_WIFI_PASSWORD="MyPassword"
//   export ANOVA_SERVER_URL="10.0.0.1:8080"
//   cargo build --release
//
// Or inline: ANOVA_WIFI_SSID=... ANOVA_WIFI_PASSWORD=... ANOVA_SERVER_URL=... cargo build --release
//
// A bare host:port for ANOVA_SERVER_URL is fine; http:// is prepended automatically.
const WIFI_SSID: &str = env!("ANOVA_WIFI_SSID");
const WIFI_PASSWORD: &str = env!("ANOVA_WIFI_PASSWORD");
const SERVER_URL: &str = env!("ANOVA_SERVER_URL");

fn normalize_server_url(url: &str) -> alloc::string::String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.into()
    } else {
        alloc::format!("http://{trimmed}")
    }
}

// How long to wait between status polls.
const POLL_INTERVAL_SECS: u64 = 10;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

// CYW43 firmware blobs, included at compile time.
static FW: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../firmware/43439A0.bin"));
static NVRAM: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../nvram_rp2040.bin"));
static CLM: &[u8] = include_bytes!("../firmware/43439A0_clm.bin");

// Receive buffer for HTTP responses.
// 8 KB is enough for the simplified /status and /recipes responses.
static mut HTTP_RX_BUF: [u8; 8192] = [0u8; 8192];

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
    // Initialize the heap allocator.
    {
        use core::mem::MaybeUninit;
        const HEAP_SIZE: usize = 32768; // 32 KB for serde_json parsing
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

    // Fetch recipes once on startup.
    #[allow(static_mut_refs)]
    let rx_buf = unsafe { &mut HTTP_RX_BUF };
    fetch_and_log_recipes(stack, rx_buf).await;

    // Poll status on a timer.
    loop {
        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut HTTP_RX_BUF };
        fetch_and_log_status(stack, rx_buf).await;
        Timer::after(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}

async fn fetch_and_log_status(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) {
    use embassy_net::dns::DnsSocket;
    use embassy_net::tcp::client::{TcpClient, TcpClientState};
    use reqwless::client::HttpClient;
    use reqwless::request::Method;

    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/status");
    let mut request = match client.request(Method::GET, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /status: connection failed");
            return;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /status: send failed");
            return;
        }
    };

    if response.status.0 != 200 {
        warn!("GET /status: HTTP {}", response.status.0);
        return;
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /status: failed to read body");
            return;
        }
    };

    match serde_json::from_slice::<anova_oven_api::OvenStatus>(body) {
        Ok(status) => {
            info!(
                "Status: mode={} temp={} target={} steam={} door={} water={}",
                status.mode.as_str(),
                status.temperature_c,
                status.target_temperature_c.unwrap_or(0.0),
                status.steam_pct,
                status.door_open,
                status.water_tank_empty,
            );
        }
        Err(_) => {
            warn!("GET /status: failed to parse JSON");
        }
    }
}

async fn fetch_and_log_recipes(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) {
    use embassy_net::dns::DnsSocket;
    use embassy_net::tcp::client::{TcpClient, TcpClientState};
    use reqwless::client::HttpClient;
    use reqwless::request::Method;

    let client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/recipes");
    let mut request = match client.request(Method::GET, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /recipes: connection failed");
            return;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /recipes: send failed");
            return;
        }
    };

    if response.status.0 != 200 {
        warn!("GET /recipes: HTTP {}", response.status.0);
        return;
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /recipes: failed to read body");
            return;
        }
    };

    match serde_json::from_slice::<alloc::vec::Vec<anova_oven_api::Recipe>>(body) {
        Ok(recipes) => {
            info!("Recipes: {} found", recipes.len());
            for r in &recipes {
                info!("  - {} ({} stages)", r.title.as_str(), r.stage_count);
            }
        }
        Err(_) => {
            warn!("GET /recipes: failed to parse JSON");
        }
    }
}
