use defmt::info;
use embassy_executor::{SpawnError, Spawner};
use embassy_rp::gpio::Input as GpioInput;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};

pub type InputChannel = Channel<CriticalSectionRawMutex, InputEvent, 4>;

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
        info!("Rotary encoder button pressed");
        channel.send(InputEvent::EncoderButton).await;

        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
pub async fn rotary_encoder_task(
    mut pin_a: GpioInput<'static>,
    mut pin_b: GpioInput<'static>,
    channel: &'static InputChannel,
) -> ! {
    const QEM: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];
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

        if (accum > 0 && dir < 0) || (accum < 0 && dir > 0) {
            accum = 0;
        }

        accum += dir;

        if accum >= TRANSITIONS_PER_DETENT {
            info!("Rotary encoder: CW");
            channel.send(InputEvent::EncoderCW).await;
            accum = 0;
        } else if accum <= -TRANSITIONS_PER_DETENT {
            info!("Rotary encoder: CCW");
            channel.send(InputEvent::EncoderCCW).await;
            accum = 0;
        }
    }
}
