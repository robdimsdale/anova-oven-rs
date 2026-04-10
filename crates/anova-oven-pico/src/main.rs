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
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Delay, Duration, Instant, Timer};
use hd44780_driver::{
    bus::FourBitBusPins,
    memory_map::MemoryMap1602,
    non_blocking::{Cursor, CursorBlink, Display, DisplayMode, HD44780},
    setup::DisplayOptions4Bit,
};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

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

const POLL_INTERVAL_SECS: u64 = 1;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

static FW: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../firmware/43439A0.bin"));
static NVRAM: &cyw43::Aligned<cyw43::A4, [u8]> =
    &cyw43::Aligned(*include_bytes!("../nvram_rp2040.bin"));
static CLM: &[u8] = include_bytes!("../firmware/43439A0_clm.bin");

static mut HTTP_RX_BUF: [u8; 16384] = [0u8; 16384];
static mut STOP_RX_BUF: [u8; 1024] = [0u8; 1024];

#[derive(Clone, Copy, defmt::Format)]
enum InputEvent {
    EncoderCW,
    EncoderCCW,
    EncoderButton,
    StopButton,
}

enum UIState {
    ShowStatus,
    BrowseRecipes { index: usize },
}

static EVENT_CHANNEL: Channel<CriticalSectionRawMutex, InputEvent, 4> = Channel::new();

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

#[embassy_executor::task]
async fn stop_button_task(stack: embassy_net::Stack<'static>, mut button: Input<'static>) -> ! {
    loop {
        button.wait_for_falling_edge().await;
        info!("Stop button pressed — sending POST /stop");
        EVENT_CHANNEL.send(InputEvent::StopButton).await;

        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut STOP_RX_BUF };
        send_stop(stack, rx_buf).await;

        // Debounce: ignore further presses for 500ms.
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
async fn rot_enc_button_task(mut button: Input<'static>) -> ! {
    loop {
        button.wait_for_falling_edge().await;
        info!("Rotary encoder button pressed");
        EVENT_CHANNEL.send(InputEvent::EncoderButton).await;

        // Debounce: ignore further presses for 500ms.
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
async fn rotary_encoder_task(mut pin_a: Input<'static>, mut pin_b: Input<'static>) -> ! {
    // Quadrature encoder lookup table.
    // Index = (prev_state << 2) | curr_state, where state = (A << 1) | B.
    // +1 = CW, -1 = CCW, 0 = no movement or invalid (bounced) transition.
    const QEM: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];

    let mut prev = ((pin_a.is_low() as u8) << 1) | (pin_b.is_low() as u8);
    // Accumulate steps: each detent produces 4 quadrature transitions.
    let mut accum: i8 = 0;

    loop {
        // Wait for either pin to change (interrupt-driven, no polling).
        embassy_futures::select::select(pin_a.wait_for_any_edge(), pin_b.wait_for_any_edge()).await;

        // Brief settle time for contact bounce.
        Timer::after(Duration::from_micros(500)).await;

        let curr = ((pin_a.is_low() as u8) << 1) | (pin_b.is_low() as u8);
        let dir = QEM[((prev << 2) | curr) as usize];
        prev = curr;
        accum += dir;

        if accum >= 4 {
            info!("Rotary encoder: CW");
            EVENT_CHANNEL.send(InputEvent::EncoderCW).await;
            accum = 0;
        } else if accum <= -4 {
            info!("Rotary encoder: CCW");
            EVENT_CHANNEL.send(InputEvent::EncoderCCW).await;
            accum = 0;
        }
    }
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

    //   LCD pins 7-10 (DB0-DB3) unconnected — 4-bit mode only uses DB4-DB7.
    let pins = FourBitBusPins {
        rs: Output::new(p.PIN_17, Level::Low), // pico physical pin 22, LCD pin 4
        en: Output::new(p.PIN_16, Level::Low), // pico physical pin 21, LCD pin 6
        d4: Output::new(p.PIN_21, Level::Low), // pico physical pin 27, LCD pin 11
        d5: Output::new(p.PIN_20, Level::Low), // pico physical pin 26, LCD pin 12
        d6: Output::new(p.PIN_19, Level::Low), // pico physical pin 25, LCD pin 13
        d7: Output::new(p.PIN_18, Level::Low), // pico physical pin 24, LCD pin 14
    };
    let mut delay = Delay;
    let options = DisplayOptions4Bit::new(MemoryMap1602::new()).with_pins(pins);
    let mut lcd = match HD44780::new(options, &mut delay).await {
        Ok(lcd) => lcd,
        Err(_) => panic!("LCD init failed"),
    };
    lcd.set_display_mode(
        DisplayMode {
            cursor_visibility: Cursor::Invisible,
            cursor_blink: CursorBlink::Off,
            display: Display::On,
        },
        &mut delay,
    )
    .await
    .ok();
    lcd.reset(&mut delay).await.ok();
    lcd.clear(&mut delay).await.ok();

    lcd.write_str("Anova Oven", &mut delay).await.ok();
    lcd.set_cursor_xy((0, 1), &mut delay).await.ok();
    lcd.write_str("Init: WIFI...", &mut delay).await.ok();

    // --- WiFi setup ---
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
    lcd.set_cursor_xy((0, 1), &mut delay).await.ok();
    lcd.write_str("Init: DHCP...", &mut delay).await.ok();

    // Wait for DHCP.
    info!("Waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("Network is up");
    if let Some(config) = stack.config_v4() {
        info!("IP address: {}", config.address);
    }

    // --- Stop button setup (GPIO 15, physical pin 20, pull-up, active low) ---
    let stop_button = Input::new(p.PIN_15, Pull::Up);
    spawner.spawn(stop_button_task(stack, stop_button).unwrap());
    info!("Stop button task spawned on GPIO 15");

    // --- Rotary Encoder button setup (GPIO 11, physical pin 15, pull-up, active low) ---
    let rot_enc_button = Input::new(p.PIN_11, Pull::Up);
    spawner.spawn(rot_enc_button_task(rot_enc_button).unwrap());
    info!("Rotary encoder button task spawned on GPIO 11");

    // --- Rotary Encoder rotation setup (GPIO 10/physical pin 14, GPIO 12, physical pin 16, pull-up) ---
    let enc_a = Input::new(p.PIN_10, Pull::Up);
    let enc_b = Input::new(p.PIN_12, Pull::Up);
    spawner.spawn(rotary_encoder_task(enc_a, enc_b).unwrap());
    info!("Rotary encoder task spawned on GPIO 10/12");

    // Fetch recipes once on startup.
    #[allow(static_mut_refs)]
    let rx_buf = unsafe { &mut HTTP_RX_BUF };
    let recipes = fetch_and_log_recipes(stack, rx_buf).await;

    // Poll status every tick, current-cook every COOK_POLL_INTERVAL ticks.
    const COOK_POLL_INTERVAL: u64 = 10;
    const INACTIVITY_TIMEOUT_SECS: u64 = 5;
    let mut tick: u64 = 0;
    let mut current_cook: Option<anova_oven_api::CurrentCook> = None;
    let mut latest_status: Option<anova_oven_api::OvenStatus> = None;
    let mut ui_state = UIState::ShowStatus;
    let mut last_input_at: Option<Instant> = None;

    loop {
        // Check inactivity timeout.
        if let Some(t) = last_input_at {
            if t.elapsed() >= Duration::from_secs(INACTIVITY_TIMEOUT_SECS) {
                info!("Inactivity timeout: reverting to ShowStatus");
                ui_state = UIState::ShowStatus;
                last_input_at = None;
            }
        }

        // Race: poll timer vs. user input.
        let poll_timer = Timer::after(Duration::from_secs(POLL_INTERVAL_SECS));
        let event_recv = EVENT_CHANNEL.receive();

        let do_poll = match embassy_futures::select::select(poll_timer, event_recv).await {
            embassy_futures::select::Either::First(()) => true,
            embassy_futures::select::Either::Second(event) => {
                last_input_at = Some(Instant::now());
                handle_input_event(event, &mut ui_state, &recipes);
                false
            }
        };

        // Periodic polling (only on timer tick).
        if do_poll {
            if tick % COOK_POLL_INTERVAL == 0 {
                #[allow(static_mut_refs)]
                let rx_buf = unsafe { &mut HTTP_RX_BUF };
                current_cook = fetch_current_cook(stack, rx_buf).await;
            }

            #[allow(static_mut_refs)]
            let rx_buf = unsafe { &mut HTTP_RX_BUF };
            if let Some(status) = fetch_and_log_status(stack, rx_buf).await {
                // Clear current_cook if oven is idle (cook ended).
                if status.mode == "idle" && current_cook.is_some() {
                    current_cook = None;
                }
                latest_status = Some(status);
            }

            tick += 1;
        }

        // Render LCD based on current UI state.
        match &ui_state {
            UIState::ShowStatus => {
                render_status_display(
                    &mut lcd,
                    &mut delay,
                    tick,
                    latest_status.as_ref(),
                    current_cook.as_ref(),
                )
                .await;
            }
            UIState::BrowseRecipes { index } => {
                render_recipe_browser(&mut lcd, &mut delay, &recipes, *index).await;
            }
        }
    }
}

fn handle_input_event(
    event: InputEvent,
    ui_state: &mut UIState,
    recipes: &[anova_oven_api::Recipe],
) {
    match event {
        InputEvent::EncoderCW => {
            if recipes.is_empty() {
                return;
            }
            match ui_state {
                UIState::ShowStatus => {
                    *ui_state = UIState::BrowseRecipes { index: 0 };
                }
                UIState::BrowseRecipes { index } => {
                    *index = (*index + 1) % recipes.len();
                }
            }
        }
        InputEvent::EncoderCCW => {
            if recipes.is_empty() {
                return;
            }
            match ui_state {
                UIState::ShowStatus => {
                    *ui_state = UIState::BrowseRecipes {
                        index: recipes.len() - 1,
                    };
                }
                UIState::BrowseRecipes { index } => {
                    *index = (*index + recipes.len() - 1) % recipes.len();
                }
            }
        }
        InputEvent::EncoderButton | InputEvent::StopButton => {
            // Resets inactivity timer (handled by caller) but no state change.
        }
    }
}

async fn render_status_display<
    B: hd44780_driver::non_blocking::bus::DataBus,
    M: hd44780_driver::memory_map::DisplayMemoryMap,
    C: hd44780_driver::charset::CharsetWithFallback,
>(
    lcd: &mut HD44780<B, M, C>,
    delay: &mut Delay,
    tick: u64,
    status: Option<&anova_oven_api::OvenStatus>,
    current_cook: Option<&anova_oven_api::CurrentCook>,
) {
    const ROTATION_PERIOD: u64 = 3;

    let Some(status) = status else {
        lcd_write_row(lcd, delay, 0, "").await;
        lcd_write_row(lcd, delay, 1, "Status: N/A").await;
        return;
    };

    let is_cooking = current_cook.is_some() && status.is_cooking();

    if is_cooking {
        let cook = current_cook.unwrap();

        // Row 0: recipe name (or "Manual cook")
        let name = cook.display_name();
        lcd_write_row(lcd, delay, 0, name).await;

        // Row 1: rotating display
        let has_timer_or_probe =
            status.timer_remaining_secs().is_some() || status.probe_temperature_c.is_some();
        let num_items: u64 = if has_timer_or_probe { 4 } else { 3 };
        let slot = (tick / ROTATION_PERIOD) % num_items;

        let phase = status.phase();

        match slot {
            0 => {
                // Stage name
                let stage = cook.current_stage(status);
                let row1 = match stage.and_then(|s| s.title.as_deref()) {
                    Some(t) => alloc::string::String::from(t),
                    None if cook.recipe_title == "[custom]" => {
                        alloc::string::String::from("Manual stage")
                    }
                    None => {
                        alloc::format!("Stage: {phase}")
                    }
                };
                lcd_write_row(lcd, delay, 1, &row1).await;
            }
            1 => {
                // Current temp → target temp (needs degree symbol)
                lcd.set_cursor_xy((0, 1), delay).await.ok();
                let current_f = celcius_to_fahrenheit(status.current_temperature_c());
                let s = alloc::format!("{:.0}", current_f);
                let mut len = s.len() + 2; // °F
                lcd.write_str(&s, delay).await.ok();
                lcd.write_byte(0xDF, delay).await.ok();
                lcd.write_str("F", delay).await.ok();
                if let Some(target_c) = status.target_temperature_c {
                    let target_f = celcius_to_fahrenheit(target_c);
                    let t = alloc::format!(">{:.0}", target_f);
                    len += t.len() + 2; // °F
                    lcd.write_str(&t, delay).await.ok();
                    lcd.write_byte(0xDF, delay).await.ok();
                    lcd.write_str("F", delay).await.ok();
                }
                for _ in len..16 {
                    lcd.write_byte(b' ', delay).await.ok();
                }
            }
            2 => {
                // Preheating or Cooking
                lcd_write_row(lcd, delay, 1, phase).await;
            }
            3 => {
                // Timer or probe
                if let Some(remaining) = status.timer_remaining_secs() {
                    let h = remaining / 3600;
                    let m = (remaining % 3600) / 60;
                    let s = remaining % 60;
                    let row1 = if h > 0 {
                        alloc::format!("Timer: {h}:{m:02}:{s:02}")
                    } else {
                        alloc::format!("Timer: {m:02}:{s:02}")
                    };
                    lcd_write_row(lcd, delay, 1, &row1).await;
                } else if let Some(probe_c) = status.probe_temperature_c {
                    // Probe current → target (needs degree symbol)
                    lcd.set_cursor_xy((0, 1), delay).await.ok();
                    let probe_f = celcius_to_fahrenheit(probe_c);
                    let s = alloc::format!("P:{:.0}", probe_f);
                    let mut len = s.len() + 2; // °F
                    lcd.write_str(&s, delay).await.ok();
                    lcd.write_byte(0xDF, delay).await.ok();
                    lcd.write_str("F", delay).await.ok();
                    let stage = cook.current_stage(status);
                    if let Some(target_c) = stage.and_then(|st| st.probe_target_c) {
                        let target_f = celcius_to_fahrenheit(target_c);
                        let t = alloc::format!(">{:.0}", target_f);
                        len += t.len() + 2;
                        lcd.write_str(&t, delay).await.ok();
                        lcd.write_byte(0xDF, delay).await.ok();
                        lcd.write_str("F", delay).await.ok();
                    }
                    for _ in len..16 {
                        lcd.write_byte(b' ', delay).await.ok();
                    }
                } else {
                    lcd_write_row(lcd, delay, 1, "--").await;
                }
            }
            _ => {}
        }
    } else {
        // No active cook — show temperature and mode/steam.
        // Row 0: oven temp + optional probe temp.
        lcd.set_cursor_xy((0, 0), delay).await.ok();
        let temp_str = alloc::format!(
            "{:.0}",
            celcius_to_fahrenheit(status.current_temperature_c())
        );
        let mut row0_len = temp_str.len() + 2; // ° + F
        lcd.write_str(&temp_str, delay).await.ok();
        lcd.write_byte(0xDF, delay).await.ok();
        lcd.write_str("F", delay).await.ok();
        if let Some(probe_c) = status.probe_temperature_c {
            let probe_str = alloc::format!(" P:{:.0}", celcius_to_fahrenheit(probe_c));
            row0_len += probe_str.len() + 2;
            lcd.write_str(&probe_str, delay).await.ok();
            lcd.write_byte(0xDF, delay).await.ok();
            lcd.write_str("F", delay).await.ok();
        }
        for _ in row0_len..16 {
            lcd.write_byte(b' ', delay).await.ok();
        }

        // Row 1: mode + optional steam target.
        let row1 = if let Some(steam) = status.steam_target_pct {
            alloc::format!("{} S:{:.0}%", status.mode, steam)
        } else {
            status.mode.clone()
        };
        lcd_write_row(lcd, delay, 1, &row1).await;
    }
}

async fn render_recipe_browser<
    B: hd44780_driver::non_blocking::bus::DataBus,
    M: hd44780_driver::memory_map::DisplayMemoryMap,
    C: hd44780_driver::charset::CharsetWithFallback,
>(
    lcd: &mut HD44780<B, M, C>,
    delay: &mut Delay,
    recipes: &[anova_oven_api::Recipe],
    index: usize,
) {
    if recipes.is_empty() {
        lcd_write_row(lcd, delay, 0, "No recipes").await;
        lcd_write_row(lcd, delay, 1, "").await;
        return;
    }

    let header = alloc::format!("Recipe {}/{}", index + 1, recipes.len());
    lcd_write_row(lcd, delay, 0, &header).await;
    lcd_write_row(lcd, delay, 1, &recipes[index].title).await;
}

/// Write a string to an LCD row, padding or truncating to 16 characters.
async fn lcd_write_row<
    B: hd44780_driver::non_blocking::bus::DataBus,
    M: hd44780_driver::memory_map::DisplayMemoryMap,
    C: hd44780_driver::charset::CharsetWithFallback,
>(
    lcd: &mut HD44780<B, M, C>,
    delay: &mut Delay,
    row: u8,
    text: &str,
) {
    lcd.set_cursor_xy((0, row), delay).await.ok();
    let len = text.len().min(16);
    lcd.write_str(&text[..len], delay).await.ok();
    for _ in len..16 {
        lcd.write_byte(b' ', delay).await.ok();
    }
}

async fn fetch_and_log_status(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) -> Option<anova_oven_api::OvenStatus> {
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
            return None;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /status: send failed");
            return None;
        }
    };

    if response.status.0 != 200 {
        warn!("GET /status: HTTP {}", response.status.0);
        return None;
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /status: failed to read body");
            return None;
        }
    };

    match serde_json::from_slice::<anova_oven_api::OvenStatus>(body) {
        Ok(status) => {
            info!(
                "Status: mode={} temp={}F target={}F steam={}% door={} water={}",
                status.mode.as_str(),
                celcius_to_fahrenheit(status.current_temperature_c()),
                celcius_to_fahrenheit(status.target_temperature_c.unwrap_or(0.0)),
                status.steam_pct,
                status.door_open,
                status.water_tank_empty,
            );
            Some(status)
        }
        Err(_) => {
            warn!("GET /status: failed to parse JSON");
            None
        }
    }
}

async fn fetch_current_cook(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) -> Option<anova_oven_api::CurrentCook> {
    use embassy_net::dns::DnsSocket;
    use embassy_net::tcp::client::{TcpClient, TcpClientState};
    use reqwless::client::HttpClient;
    use reqwless::request::Method;

    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/current-cook");
    let mut request = match client.request(Method::GET, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /current-cook: connection failed");
            return None;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /current-cook: send failed");
            return None;
        }
    };

    if response.status.0 == 204 {
        return None; // No active cook.
    }
    if response.status.0 != 200 {
        warn!("GET /current-cook: HTTP {}", response.status.0);
        return None;
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /current-cook: failed to read body");
            return None;
        }
    };

    match serde_json::from_slice::<anova_oven_api::CurrentCook>(body) {
        Ok(cook) => {
            info!(
                "Current cook: {} ({} stages)",
                cook.recipe_title.as_str(),
                cook.total_stage_count,
            );
            Some(cook)
        }
        Err(_) => {
            warn!("GET /current-cook: failed to parse JSON");
            None
        }
    }
}

async fn send_stop(stack: embassy_net::Stack<'static>, rx_buf: &mut [u8]) {
    use embassy_net::dns::DnsSocket;
    use embassy_net::tcp::client::{TcpClient, TcpClientState};
    use reqwless::client::HttpClient;
    use reqwless::request::Method;

    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/stop");
    let mut request = match client.request(Method::POST, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("POST /stop: connection failed");
            return;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("POST /stop: send failed");
            return;
        }
    };

    if response.status.0 >= 200 && response.status.0 < 300 {
        info!("POST /stop: success (HTTP {})", response.status.0);
    } else {
        warn!("POST /stop: HTTP {}", response.status.0);
    }
}

async fn fetch_and_log_recipes(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) -> alloc::vec::Vec<anova_oven_api::Recipe> {
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
            return alloc::vec::Vec::new();
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /recipes: send failed");
            return alloc::vec::Vec::new();
        }
    };

    if response.status.0 != 200 {
        warn!("GET /recipes: HTTP {}", response.status.0);
        return alloc::vec::Vec::new();
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /recipes: failed to read body");
            return alloc::vec::Vec::new();
        }
    };

    match serde_json::from_slice::<alloc::vec::Vec<anova_oven_api::Recipe>>(body) {
        Ok(recipes) => {
            info!("Recipes: {} found", recipes.len());
            for r in &recipes {
                info!("  - {} ({} stages)", r.title.as_str(), r.stage_count);
            }
            recipes
        }
        Err(_) => {
            warn!("GET /recipes: failed to parse JSON");
            alloc::vec::Vec::new()
        }
    }
}

fn celcius_to_fahrenheit(c: f32) -> f32 {
    c * 1.8 + 32.0
}
