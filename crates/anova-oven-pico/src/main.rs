#![no_std]
#![no_main]

extern crate alloc;

mod http;
mod rng;
mod ws;

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

// WiFi credentials — in a real deployment these would come from flash/config.
const WIFI_SSID: &str = "YOUR_WIFI_SSID";
const WIFI_PASSWORD: &str = "YOUR_WIFI_PASSWORD";

// Anova API token — in a real deployment, read from flash.
const ANOVA_TOKEN: &str = "YOUR_ANOVA_TOKEN";

// Firebase credentials — in a real deployment, read from flash.
const ANOVA_EMAIL: &str = "YOUR_EMAIL";
const ANOVA_PASSWORD: &str = "YOUR_PASSWORD";

// TLS record buffers (shared between HTTP and WebSocket phases).
// 16,640 bytes each — the TLS 1.3 maximum record size.
// These are static because they're too large for the stack.
static mut TLS_READ_BUF: [u8; 16640] = [0; 16640];
static mut TLS_WRITE_BUF: [u8; 16640] = [0; 16640];

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
        const HEAP_SIZE: usize = 32768;
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

    // Phase 1: Firestore recipe fetch via HTTPS (reqwless + embedded-tls).
    info!("--- Firestore HTTP ---");
    #[allow(static_mut_refs)]
    let (tls_read, tls_write) = unsafe { (&mut TLS_READ_BUF, &mut TLS_WRITE_BUF) };

    http::run_firestore_flow(stack, tls_read, tls_write, ANOVA_EMAIL, ANOVA_PASSWORD).await;

    // Phase 2: WebSocket connection for oven control (embedded-tls + websocketz).
    info!("--- WebSocket ---");
    #[allow(static_mut_refs)]
    let (tls_read, tls_write) = unsafe { (&mut TLS_READ_BUF, &mut TLS_WRITE_BUF) };

    ws::run_websocket(stack, ANOVA_TOKEN, tls_read, tls_write).await;

    // Keep running if WebSocket disconnects.
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
