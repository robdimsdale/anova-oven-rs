#![no_std]
#![no_main]

extern crate alloc;

mod api;
mod app_state;
mod backlight;
mod display;
mod events;
mod logic;

use embedded_alloc::LlffHeap as Heap;

#[global_allocator]
static HEAP: Heap = Heap::empty();

use cyw43_pio::PioSpi;
use defmt::{debug, info, warn};
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_time::{Delay, Duration, Instant, Timer};
use hd44780_driver::{
    bus::FourBitBusPins, memory_map::MemoryMap1602, non_blocking::HD44780,
    setup::DisplayOptions4Bit,
};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use crate::app_state::AppState;
use crate::backlight::BacklightController;
use crate::display::LcdController;
use crate::events::{rot_enc_button_task, rotary_encoder_task, EVENT_CHANNEL};

const WIFI_SSID: &str = env!("ANOVA_WIFI_SSID");
const WIFI_PASSWORD: &str = env!("ANOVA_WIFI_PASSWORD");
pub(crate) const SERVER_URL: &str = env!("ANOVA_SERVER_URL");

const POLL_INTERVAL_SECS: u64 = 1;
const DISPLAY_REFRESH_MS: u64 = 50;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

static FW: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../firmware/43439A0.bin"));
static NVRAM: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../nvram_rp2040.bin"));
static CLM: &[u8] = include_bytes!("../firmware/43439A0_clm.bin");

pub(crate) static mut HTTP_RX_BUF: [u8; 16384] = [0u8; 16384];

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
    // Init heap
    use core::mem::MaybeUninit;

    const HEAP_SIZE: usize = 32768;
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

    #[allow(static_mut_refs)]
    unsafe {
        HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE);
    }

    // Init peripherals
    let p = embassy_rp::init(Default::default());

    // Init LCD
    let mut lcd_delay = Delay;
    let lcd = match HD44780::new(
        DisplayOptions4Bit::new(MemoryMap1602::new()).with_pins(FourBitBusPins {
            rs: Output::new(p.PIN_17, Level::Low),
            en: Output::new(p.PIN_16, Level::Low),
            d4: Output::new(p.PIN_21, Level::Low),
            d5: Output::new(p.PIN_20, Level::Low),
            d6: Output::new(p.PIN_19, Level::Low),
            d7: Output::new(p.PIN_18, Level::Low),
        }),
        &mut lcd_delay,
    )
    .await
    {
        Ok(lcd) => lcd,
        Err(_) => panic!("LCD init failed"),
    };

    let mut lcd_controller = LcdController::new(lcd, lcd_delay);
    lcd_controller.configure().await;

    let mut app = AppState::new(
        BacklightController::new(p.PWM_SLICE3, p.PIN_6, p.PIN_7, p.PWM_SLICE4, p.PIN_8),
        lcd_controller,
    );

    // Spawn tasks for user input handling.
    spawner.spawn(rot_enc_button_task(Input::new(p.PIN_11, Pull::Up)).unwrap());
    info!("Rotary encoder button task spawned on GPIO 11");

    spawner.spawn(
        rotary_encoder_task(
            Input::new(p.PIN_9, Pull::Up),
            Input::new(p.PIN_10, Pull::Up),
        )
        .unwrap(),
    );
    info!("Rotary encoder task spawned on GPIO 9/10");

    // Initialize wifi
    app.render_wifi_init().await;

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

    let config = Config::dhcpv4(Default::default());
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let seed: u64 = 0x0123_4567_89ab_cdef;
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());

    info!("Connecting to WiFi: {}", WIFI_SSID);
    info!("Configured server URL: {}", SERVER_URL);
    if SERVER_URL.contains("localhost") || SERVER_URL.contains("127.0.0.1") {
        warn!("ANOVA_SERVER_URL points to loopback; Pico cannot reach your laptop via localhost");
    }
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

    app.render_dhcp_init().await;

    info!("Waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("Network is up");
    if let Some(config) = stack.config_v4() {
        info!("IP address: {}", config.address);
    }

    app.init_data(stack).await;
    app.render_current_view().await;

    info!("Init complete, entering main loop");

    let mut next_poll_at = Instant::now() + Duration::from_secs(POLL_INTERVAL_SECS);

    loop {
        debug!("--- Main loop tick {} ---", app.tick);
        app.update_inactivity_timeout();

        let display_timer = Timer::after(Duration::from_millis(DISPLAY_REFRESH_MS));
        let event_recv = EVENT_CHANNEL.receive();

        match embassy_futures::select::select(display_timer, event_recv).await {
            embassy_futures::select::Either::First(()) => {
                // This branch is a no-op; we just want to trigger a display refresh at a regular interval for smoother scrolling,
                // independent of status polling or user input. The display task will run at the end of the loop unconditionally, so no action is needed here.
            }
            embassy_futures::select::Either::Second(event) => {
                app.handle_user_activity(event).await;
            }
        }

        // Render first so optimistic UI transitions are visible immediately.
        app.render_current_view().await;

        // TODO: can we split out the status update API call into the same queue? Should we?
        app.process_pending_api_action(stack).await;
        app.process_queued_refresh_if_due(stack).await;

        if Instant::now() >= next_poll_at {
            app.poll_status_if_due(stack).await;
            next_poll_at += Duration::from_secs(POLL_INTERVAL_SECS);
        }

        app.render_current_view().await;
    }
}
