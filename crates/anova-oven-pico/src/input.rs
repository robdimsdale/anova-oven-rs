use anova_oven_pico_core::button::{ButtonVerdict, Debouncer};
use anova_oven_pico_core::encoder::{EncoderTick, QuadratureDecoder};
use defmt::{info, warn};
use embassy_executor::{SpawnError, Spawner};
use embassy_rp::gpio::Input as GpioInput;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};

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
    let sample_interval = Duration::from_millis(Debouncer::SAMPLE_INTERVAL_MS);

    loop {
        // Level-triggered, so it returns immediately if the line is already
        // low: unlike an edge wait, nothing is missed in the window between
        // one excursion ending and the next arming.
        button.wait_for_low().await;

        let mut debouncer = Debouncer::new();
        let excursion_start = Instant::now();
        let mut last_sample = excursion_start;

        loop {
            Timer::after(sample_interval).await;
            // Measure the interval rather than assuming it. The display
            // task flushes SPI blocking on this same executor, so a sample
            // could in principle land late and has to carry that weight.
            // Measured cadence says that isn't happening today — this is
            // insurance (see `anova_oven_pico_core::button`).
            let now = Instant::now();
            let elapsed_ms = now.duration_since(last_sample).as_millis() as u32;
            last_sample = now;

            match debouncer.sample(button.is_low(), elapsed_ms) {
                ButtonVerdict::Idle => continue,
                ButtonVerdict::Pressed => {
                    #[cfg(feature = "verbose-logs")]
                    info!("Rotary encoder button pressed");
                    if channel.try_send(InputEvent::EncoderButton).is_err() {
                        warn!("Input channel full; dropping encoder button event");
                    }
                }
                ButtonVerdict::Released => {
                    // Contact time, not the excursion: the integrator has to
                    // discharge before it reports a release, so the excursion
                    // runs to roughly twice the time the button was actually
                    // down.
                    #[cfg(feature = "verbose-logs")]
                    info!(
                        "Rotary encoder button released after {}ms of contact",
                        debouncer.contact_ms()
                    );
                    break;
                }
                ButtonVerdict::Glitch => {
                    // Rotating the shaft couples noise onto the switch line
                    // (see `anova_oven_pico_core::button`). Rejected without
                    // an event; re-arm immediately so a real press that only
                    // chattered its way here is caught on the next pass.
                    //
                    // Two diagnostics, because two different things look like
                    // this. `samples` vs the excursion tells starvation (far
                    // fewer samples than the elapsed time allows) from real
                    // chatter; `peak` at roughly half the excursion is the
                    // signature of a clean press that was simply shorter than
                    // PRESS_MS, which means the threshold is miscalibrated
                    // rather than the line being noisy.
                    #[cfg(feature = "verbose-logs")]
                    info!(
                        "Rotary encoder button rejected: {}ms contact, {}ms excursion, {} samples, peak {}ms of {}ms",
                        debouncer.contact_ms(),
                        excursion_start.elapsed().as_millis(),
                        debouncer.samples(),
                        debouncer.peak_charge_ms(),
                        Debouncer::PRESS_MS,
                    );
                    break;
                }
            }
        }
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
