use anova_oven_api::Recipe;
use defmt::info;
use embassy_net::Stack;
use embassy_rp::gpio::Input;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};

use crate::api::send_stop;

#[derive(Clone, Copy, defmt::Format)]
pub enum InputEvent {
    EncoderCW,
    EncoderCCW,
    EncoderButton,
    StopButton,
}

pub enum UIState {
    ShowStatus,
    BrowseRecipes { index: usize },
}

pub static EVENT_CHANNEL: Channel<CriticalSectionRawMutex, InputEvent, 4> = Channel::new();

static mut STOP_RX_BUF: [u8; 1024] = [0u8; 1024];

#[embassy_executor::task]
pub async fn stop_button_task(stack: Stack<'static>, mut button: Input<'static>) -> ! {
    loop {
        button.wait_for_falling_edge().await;
        info!("Stop button pressed - sending POST /stop");
        EVENT_CHANNEL.send(InputEvent::StopButton).await;

        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut STOP_RX_BUF };
        send_stop(stack, rx_buf).await;

        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
pub async fn rot_enc_button_task(mut button: Input<'static>) -> ! {
    loop {
        button.wait_for_falling_edge().await;
        info!("Rotary encoder button pressed");
        EVENT_CHANNEL.send(InputEvent::EncoderButton).await;

        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
pub async fn rotary_encoder_task(mut pin_a: Input<'static>, mut pin_b: Input<'static>) -> ! {
    const QEM: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];

    let mut prev = ((pin_a.is_low() as u8) << 1) | (pin_b.is_low() as u8);
    let mut accum: i8 = 0;

    loop {
        embassy_futures::select::select(pin_a.wait_for_any_edge(), pin_b.wait_for_any_edge()).await;
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

pub fn handle_input_event(event: InputEvent, ui_state: &mut UIState, recipes: &[Recipe]) {
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
            // Caller handles inactivity timer reset.
        }
    }
}
