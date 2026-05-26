use anova_oven_pico_core::encoder::{EncoderTick, QuadratureDecoder};
use defmt::{info, warn};
use embassy_executor::{SpawnError, Spawner};
use embassy_rp::gpio::Input as GpioInput;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};

pub type InputChannel = Channel<CriticalSectionRawMutex, InputEvent, 16>;

#[derive(Clone, Copy, defmt::Format)]
pub enum InputEvent {
    EncoderCW,
    EncoderCCW,
    EncoderButton,
}

pub struct Input<'a>(&'a InputChannel);

impl<'a> Input<'a> {
    pub fn new(
        pin_a: GpioInput<'static>,
        pin_b: GpioInput<'static>,
        button: GpioInput<'static>,
        channel: &'static InputChannel,
        spawner: Spawner,
    ) -> Result<Self, SpawnError> {
        spawner.spawn(rot_enc_button_task(button, channel)?);
        info!("Rotary encoder button task spawned on GPIO 11");
        spawner.spawn(rotary_encoder_task(pin_a, pin_b, channel)?);
        info!("Rotary encoder task spawned on GPIO 9/10");
        Ok(Self(channel))
    }

    pub async fn recv(&self) -> InputEvent {
        self.0.receive().await
    }
}

#[embassy_executor::task]
pub async fn rot_enc_button_task(
    mut button: GpioInput<'static>,
    channel: &'static InputChannel,
) -> ! {
    loop {
        button.wait_for_falling_edge().await;
        #[cfg(feature = "verbose-logs")]
        info!("Rotary encoder button pressed");
        if channel.try_send(InputEvent::EncoderButton).is_err() {
            warn!("Input channel full; dropping encoder button event");
        }

        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
pub async fn rotary_encoder_task(
    mut pin_a: GpioInput<'static>,
    mut pin_b: GpioInput<'static>,
    channel: &'static InputChannel,
) -> ! {
    let mut decoder = QuadratureDecoder::new(pin_a.is_low(), pin_b.is_low());

    loop {
        embassy_futures::select::select(pin_a.wait_for_any_edge(), pin_b.wait_for_any_edge()).await;
        Timer::after(Duration::from_micros(500)).await;

        let Some(tick) = decoder.update(pin_a.is_low(), pin_b.is_low()) else {
            continue;
        };

        let event = match tick {
            EncoderTick::Cw => InputEvent::EncoderCW,
            EncoderTick::Ccw => InputEvent::EncoderCCW,
        };
        #[cfg(feature = "verbose-logs")]
        info!(
            "Rotary encoder: {}",
            match tick {
                EncoderTick::Cw => "CW",
                EncoderTick::Ccw => "CCW",
            }
        );
        if channel.try_send(event).is_err() {
            let direction = match tick {
                EncoderTick::Cw => "CW",
                EncoderTick::Ccw => "CCW",
            };
            warn!("Input channel full; dropping encoder {} event", direction);
        }
    }
}
