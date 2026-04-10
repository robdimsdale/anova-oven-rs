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
use embassy_rp::pwm::{Config as PwmConfig, Pwm};
use embassy_time::{Delay, Duration, Timer};
use hd44780_driver::{
    bus::FourBitBusPins, memory_map::MemoryMap1602, non_blocking::HD44780,
    setup::DisplayOptions4Bit,
};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use crate::api::{fetch_and_log_recipes, fetch_and_log_status, fetch_current_cook};
use crate::app_state::AppState;
use crate::backlight::BacklightController;
use crate::display::{configure_lcd_display, render_status_display};
use crate::events::{
    rot_enc_button_task, rotary_encoder_task, stop_button_task, EVENT_CHANNEL,
};

const WIFI_SSID: &str = env!("ANOVA_WIFI_SSID");
const WIFI_PASSWORD: &str = env!("ANOVA_WIFI_PASSWORD");
pub(crate) const SERVER_URL: &str = env!("ANOVA_SERVER_URL");

const POLL_INTERVAL_SECS: u64 = 1;
pub(crate) const INACTIVITY_TIMEOUT_SECS: u64 = 5;
pub(crate) const LED_DIM_TIMER_SECS: u64 = 5;
pub(crate) const COOK_POLL_INTERVAL: u64 = 10;

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

fn init_heap() {
    use core::mem::MaybeUninit;

    const HEAP_SIZE: usize = 32768;
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

    #[allow(static_mut_refs)]
    unsafe {
        HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE);
    }
}

fn init_backlight_pwm(
    pwm_slice3: embassy_rp::Peri<'static, embassy_rp::peripherals::PWM_SLICE3>,
    pin6: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_6>,
    pin7: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_7>,
    pwm_slice4: embassy_rp::Peri<'static, embassy_rp::peripherals::PWM_SLICE4>,
    pin8: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_8>,
) -> BacklightController {
    let mut backlight_cfg = PwmConfig::default();
    backlight_cfg.top = 0x8000;
    backlight_cfg.invert_a = true;
    backlight_cfg.invert_b = true;
    backlight_cfg.compare_b = 0;

    let pwm_red_green = Pwm::new_output_ab(pwm_slice3, pin6, pin7, backlight_cfg.clone());

    backlight_cfg.compare_a = 0;
    backlight_cfg.compare_b = 0;
    let pwm_blue = Pwm::new_output_a(pwm_slice4, pin8, backlight_cfg);

    BacklightController::new(pwm_red_green, pwm_blue)
}

async fn connect_wifi(control: &mut cyw43::Control<'static>) {
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
}

async fn wait_for_dhcp(stack: embassy_net::Stack<'static>) {
    info!("Waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("Network is up");
    if let Some(config) = stack.config_v4() {
        info!("IP address: {}", config.address);
    }
}

fn spawn_input_tasks(
    spawner: &Spawner,
    stack: embassy_net::Stack<'static>,
    stop_button_pin: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_15>,
    rot_enc_button_pin: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_11>,
    enc_a_pin: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_9>,
    enc_b_pin: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_10>,
) {
    let stop_button = Input::new(stop_button_pin, Pull::Up);
    spawner.spawn(stop_button_task(stack, stop_button).unwrap());
    info!("Stop button task spawned on GPIO 15");

    let rot_enc_button = Input::new(rot_enc_button_pin, Pull::Up);
    spawner.spawn(rot_enc_button_task(rot_enc_button).unwrap());
    info!("Rotary encoder button task spawned on GPIO 11");

    let enc_a = Input::new(enc_a_pin, Pull::Up);
    let enc_b = Input::new(enc_b_pin, Pull::Up);
    spawner.spawn(rotary_encoder_task(enc_a, enc_b).unwrap());
    info!("Rotary encoder task spawned on GPIO 9/10");
}

async fn fetch_initial_data(
    stack: embassy_net::Stack<'static>,
) -> (
    alloc::vec::Vec<anova_oven_api::Recipe>,
    Option<anova_oven_api::CurrentCook>,
    Option<anova_oven_api::OvenStatus>,
) {
    #[allow(static_mut_refs)]
    let rx_buf = unsafe { &mut HTTP_RX_BUF };
    let recipes = fetch_and_log_recipes(stack, rx_buf).await;

    #[allow(static_mut_refs)]
    let rx_buf = unsafe { &mut HTTP_RX_BUF };
    let current_cook = fetch_current_cook(stack, rx_buf).await;

    #[allow(static_mut_refs)]
    let rx_buf = unsafe { &mut HTTP_RX_BUF };
    let latest_status = fetch_and_log_status(stack, rx_buf).await;

    (recipes, current_cook, latest_status)
}

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
    init_heap();

    let p = embassy_rp::init(Default::default());
    let mut delay = Delay;

    let mut lcd = match HD44780::new(
        DisplayOptions4Bit::new(MemoryMap1602::new()).with_pins(FourBitBusPins {
            rs: Output::new(p.PIN_17, Level::Low),
            en: Output::new(p.PIN_16, Level::Low),
            d4: Output::new(p.PIN_21, Level::Low),
            d5: Output::new(p.PIN_20, Level::Low),
            d6: Output::new(p.PIN_19, Level::Low),
            d7: Output::new(p.PIN_18, Level::Low),
        }),
        &mut delay,
    )
    .await
    {
        Ok(lcd) => lcd,
        Err(_) => panic!("LCD init failed"),
    };
    configure_lcd_display(&mut lcd, &mut delay).await;

    let mut backlight =
        init_backlight_pwm(p.PWM_SLICE3, p.PIN_6, p.PIN_7, p.PWM_SLICE4, p.PIN_8);
    backlight.set_full();

    lcd.write_str("Anova Oven", &mut delay).await.ok();
    lcd.set_cursor_xy((0, 1), &mut delay).await.ok();
    lcd.write_str("Init: WIFI...", &mut delay).await.ok();

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

    connect_wifi(&mut control).await;

    lcd.set_cursor_xy((0, 1), &mut delay).await.ok();
    lcd.write_str("Init: DHCP...", &mut delay).await.ok();

    wait_for_dhcp(stack).await;

    spawn_input_tasks(&spawner, stack, p.PIN_15, p.PIN_11, p.PIN_9, p.PIN_10);

    let (recipes, current_cook, latest_status) = fetch_initial_data(stack).await;
    let mut app = AppState::new(current_cook, latest_status);

    info!("Init complete, dimming backlight and entering main loop");
    backlight.set_dim();

    render_status_display(
        &mut lcd,
        &mut delay,
        app.tick,
        app.latest_status.as_ref(),
        app.current_cook.as_ref(),
    )
    .await;

    loop {
        debug!("--- Main loop tick {} ---", app.tick);
        app.update_inactivity_timeout();

        let poll_timer = Timer::after(Duration::from_secs(POLL_INTERVAL_SECS));
        let event_recv = EVENT_CHANNEL.receive();

        match embassy_futures::select::select(poll_timer, event_recv).await {
            embassy_futures::select::Either::First(()) => {
                app.poll_status_if_due(stack).await;
            }
            embassy_futures::select::Either::Second(event) => {
                app.handle_user_activity(event, &recipes, &mut backlight);
            }
        }
        app.apply_backlight_policy(&mut backlight);
        app.render_current_view(&mut lcd, &mut delay, &recipes).await;
    }
}
