use anova_oven_api::Recipe;
use defmt::info;
use embassy_rp::gpio::Input;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Instant;
use embassy_time::{Duration, Timer};

#[derive(Clone, Copy, defmt::Format)]
pub enum InputEvent {
    EncoderCW,
    EncoderCCW,
    EncoderButton,
}

pub enum UIState {
    ShowStatus,
    BrowseRecipes { index: usize },
    ConfirmStopCooking { last_input_at: Instant },
}

pub static EVENT_CHANNEL: Channel<CriticalSectionRawMutex, InputEvent, 4> = Channel::new();

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
    // This encoder produces a full 4-transition quadrature cycle per tactile detent.
    const TRANSITIONS_PER_DETENT: i8 = 4;

    let mut prev = ((pin_a.is_low() as u8) << 1) | (pin_b.is_low() as u8);
    let mut accum: i8 = 0;

    loop {
        embassy_futures::select::select(pin_a.wait_for_any_edge(), pin_b.wait_for_any_edge()).await;
        Timer::after(Duration::from_micros(500)).await;

        let curr = ((pin_a.is_low() as u8) << 1) | (pin_b.is_low() as u8);
        let dir = QEM[((prev << 2) | curr) as usize];
        prev = curr;

        if dir == 0 {
            continue;
        }

        // Avoid carrying stale half-steps across a direction reversal.
        if (accum > 0 && dir < 0) || (accum < 0 && dir > 0) {
            accum = 0;
        }

        accum += dir;

        if accum >= TRANSITIONS_PER_DETENT {
            info!("Rotary encoder: CW");
            EVENT_CHANNEL.send(InputEvent::EncoderCW).await;
            accum = 0;
        } else if accum <= -TRANSITIONS_PER_DETENT {
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
                UIState::ConfirmStopCooking { .. } => {
                    // Caller owns confirm-stop transition logic.
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
                UIState::ConfirmStopCooking { .. } => {
                    // Caller owns confirm-stop transition logic.
                }
            }
        }
        InputEvent::EncoderButton => {
            // Caller handles inactivity timer reset.
        }
    }
}
