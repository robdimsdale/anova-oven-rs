#![no_std]
#![no_main]

extern crate alloc;

mod api;
mod api_client;
mod backlight;
mod display;
mod input;
mod lcd;
mod persist;
mod state;

use embedded_alloc::LlffHeap as Heap;

#[global_allocator]
static HEAP: Heap = Heap::empty();

use cyw43_pio::PioSpi;
use defmt::{info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input as GpioInput, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_rp::watchdog::Watchdog;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use embassy_time::{with_timeout, Delay, Duration, Instant, Timer};
use hd44780_driver::{
    bus::FourBitBusPins, memory_map::MemoryMap1602, non_blocking::HD44780,
    setup::DisplayOptions4Bit,
};
use static_cell::StaticCell;

use crate::api_client::{ApiClient, CommandChannel, StateWatch};
use crate::backlight::BacklightController;
use crate::display::{Display, DisplayNotifier, ViewSpec};
use crate::input::{Input, InputChannel};
use crate::lcd::LcdController;
use crate::state::{AppState, Ctx};

const WIFI_SSID: &str = env!("ANOVA_WIFI_SSID");
const WIFI_PASSWORD: &str = env!("ANOVA_WIFI_PASSWORD");
pub(crate) const SERVER_URL: &str = env!("ANOVA_SERVER_URL");

const WATCHDOG_TIMEOUT_SECS: u64 = 8;
const WATCHDOG_FEED_INTERVAL_SECS: u64 = 2;
/// Bring-up deadlines. The WiFi join and DHCP waits used to be unbounded
/// loops; with the watchdog feeder running unconditionally, a stall here
/// (e.g. associated to the AP but no DHCP lease) hangs the box forever
/// instead of recovering. On timeout we deliberately reboot and retry
/// from a clean slate (see `persist::reboot_init_timeout`). Generous
/// values: a real join can take >10s and slow networks lease slowly;
/// we only want to catch genuine "never going to happen" stalls.
const WIFI_JOIN_DEADLINE_SECS: u64 = 45;
const DHCP_DEADLINE_SECS: u64 = 45;
const RECOVERY_DISPLAY_SECS: u64 = 30;
const RECOVERY_RENDER_TICK_MS: u64 = 50;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

static FW: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../firmware/43439A0.bin"));
static NVRAM: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../nvram_rp2040.bin"));
static CLM: &[u8] = include_bytes!("../firmware/43439A0_clm.bin");
static DISPLAY_NOTIFIER: DisplayNotifier = Signal::new();
static INPUT_CHANNEL: InputChannel = Channel::new();
static API_COMMANDS: CommandChannel = Channel::new();
static API_STATE: StateWatch = Watch::new();

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[cfg(feature = "verbose-logs")]
#[embassy_executor::task]
async fn heap_monitor_task() -> ! {
    let mut peak_used = 0usize;
    loop {
        let used = HEAP.used();
        let free = HEAP.free();
        if used > peak_used {
            peak_used = used;
        }
        info!("heap: used={} free={} peak_used={}", used, free, peak_used);
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn watchdog_feeder_task(mut watchdog: Watchdog) -> ! {
    loop {
        watchdog.feed(Duration::from_secs(WATCHDOG_TIMEOUT_SECS));
        persist::bump_watchdog_heartbeat();
        persist::record_uptime_secs(Instant::now().as_secs() as u32);
        persist::record_free_heap(HEAP.free() as u32);
        Timer::after(Duration::from_secs(WATCHDOG_FEED_INTERVAL_SECS)).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    use core::mem::MaybeUninit;

    const HEAP_SIZE: usize = 32768;
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

    #[allow(static_mut_refs)]
    unsafe {
        HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE);
    }

    // Read persisted reset/panic counters and any stored panic message.
    // This bumps reset_count so subsequent boots can tell if the previous
    // run ended cleanly or not. Must run after the allocator is up because
    // the snapshot owns a heapless::String backed by stack memory only, but
    // before peripheral init so we record the boot regardless of what comes
    // next.
    let recovery = persist::init_at_boot();

    let p = embassy_rp::init(Default::default());

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

    // Log the persisted counters, breadcrumbs, and message every boot so
    // an attached probe sees them immediately, regardless of whether
    // this boot is doing a recovery flash.
    info!(
        "persist: reset_reason={} reset_count={} panic_count={} message_is_new={} msg_len={}",
        recovery.reset_reason,
        recovery.reset_count,
        recovery.panic_count,
        recovery.message_is_new,
        recovery.message.as_deref().map(|s| s.len()).unwrap_or(0),
    );
    info!(
        "persist: last_app_state={} last_uptime_secs={} api_hb={} display_hb={} watchdog_hb={}",
        recovery.last_app_state,
        recovery.last_uptime_secs,
        recovery.api_heartbeat,
        recovery.display_heartbeat,
        recovery.watchdog_heartbeat,
    );
    for (i, entry) in recovery.reset_history.iter().enumerate() {
        info!(
            "persist: reset_history[{}]: reason={} uptime_secs={} api_hb={} free_heap={} net_up={} api_fail={}",
            i,
            entry.reset_reason,
            entry.uptime_secs,
            entry.api_heartbeat,
            entry.free_heap,
            entry.network_up,
            entry.api_fail_count,
        );
    }
    if let Some(msg) = recovery.message.as_deref() {
        info!("persist: last panic message: {}", msg);
    }

    // Only flash the LCD recovery view when a new panic has occurred
    // since we last displayed one. Watchdog/external resets without a
    // fresh panic message don't get a 30-second display — the counters
    // are still readable via probe-rs at any time.
    if recovery.message_is_new {
        warn!(
            "New panic since last display: panic_count={} reset_count={}",
            recovery.panic_count, recovery.reset_count,
        );
        show_recovery_view(&mut lcd_controller, &recovery).await;
        persist::mark_displayed();
    }

    // Start the hardware watchdog only after the recovery display window
    // has expired so a human has time to read it. The feeder task fires
    // every WATCHDOG_FEED_INTERVAL_SECS and the timeout is set to
    // WATCHDOG_TIMEOUT_SECS, giving a comfortable margin under normal load
    // while still catching freezes within seconds.
    let mut watchdog = Watchdog::new(p.WATCHDOG);
    watchdog.enable_tick_generation(12); // clk_ref is 12 MHz
    watchdog.pause_on_debug(true);
    watchdog.start(Duration::from_secs(WATCHDOG_TIMEOUT_SECS));
    spawner.spawn(watchdog_feeder_task(watchdog).unwrap());

    let display = Display::new(lcd_controller, &DISPLAY_NOTIFIER, spawner).unwrap();

    #[cfg(feature = "verbose-logs")]
    spawner.spawn(heap_monitor_task().unwrap());

    let backlight_controller =
        BacklightController::new(p.PWM_SLICE3, p.PIN_6, p.PIN_7, p.PWM_SLICE4, p.PIN_8);

    let input = Input::new(
        GpioInput::new(p.PIN_9, Pull::Up),
        GpioInput::new(p.PIN_10, Pull::Up),
        GpioInput::new(p.PIN_11, Pull::Up),
        &INPUT_CHANNEL,
        spawner,
    )
    .unwrap();

    info!("Initializing WiFi...");
    display.render(ViewSpec::WifiInit);

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
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;

    let config = Config::dhcpv4(Default::default());
    static RESOURCES: StaticCell<StackResources<16>> = StaticCell::new();
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
        warn!("ANOVA_SERVER_URL points to loopback");
    }
    // Bound the WiFi join: retry forever *within* the deadline, then
    // deliberately reboot. The breadcrumb is set before the wait so a
    // timeout (or any reset while stuck here) is attributed to
    // ResetReason::InitTimeout with stage=WiFi on the next boot.
    persist::record_app_state(persist::INIT_STAGE_WIFI);
    let joined = with_timeout(Duration::from_secs(WIFI_JOIN_DEADLINE_SECS), async {
        loop {
            match control
                .join(WIFI_SSID, cyw43::JoinOptions::new(WIFI_PASSWORD.as_bytes()))
                .await
            {
                Ok(_) => break,
                Err(err) => {
                    warn!("WiFi join failed: {}", defmt::Debug2Format(&err));
                    Timer::after(Duration::from_secs(1)).await;
                }
            }
        }
    })
    .await;
    if joined.is_err() {
        warn!(
            "WiFi join did not succeed within {}s; rebooting",
            WIFI_JOIN_DEADLINE_SECS
        );
        persist::reboot_init_timeout();
    }
    info!("WiFi connected");

    info!("Waiting for DHCP...");
    display.render(ViewSpec::DhcpInit);

    // Same pattern for DHCP — this is the wait that actually stalled in
    // the field (associated to the AP but never got a lease).
    persist::record_app_state(persist::INIT_STAGE_DHCP);
    let configured = with_timeout(Duration::from_secs(DHCP_DEADLINE_SECS), async {
        while !stack.is_config_up() {
            Timer::after(Duration::from_millis(100)).await;
        }
    })
    .await;
    if configured.is_err() {
        warn!(
            "DHCP did not complete within {}s; rebooting",
            DHCP_DEADLINE_SECS
        );
        persist::reboot_init_timeout();
    }
    info!("Network is up");
    persist::record_network_up();
    if let Some(config) = stack.config_v4() {
        info!("IP address: {}", defmt::Display2Format(&config.address));
    }

    let api = ApiClient::new(stack, &API_COMMANDS, &API_STATE, spawner).unwrap();
    let api_rx = api.receiver().unwrap();
    let mut ctx = Ctx {
        input: &input,
        api: &api,
        api_rx,
        display: &display,
        backlight: backlight_controller,
    };
    let mut state = AppState::default();

    info!("Init complete, entering main loop");

    loop {
        state = state.execute(&mut ctx).await;
    }
}

async fn show_recovery_view(lcd: &mut LcdController, recovery: &persist::Snapshot) {
    let view = ViewSpec::Recovery {
        reset_count: recovery.reset_count,
        panic_count: recovery.panic_count,
        message: recovery.message.as_deref().map(alloc::string::String::from),
    };
    let deadline = Instant::now() + Duration::from_secs(RECOVERY_DISPLAY_SECS);
    while Instant::now() < deadline {
        lcd.render(&view).await;
        Timer::after(Duration::from_millis(RECOVERY_RENDER_TICK_MS)).await;
    }
}
