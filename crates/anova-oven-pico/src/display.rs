use embassy_executor::{SpawnError, Spawner};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};

pub use anova_oven_pico_core::fsm::ViewSpec;

use crate::lcd::LcdController;

const ANIM_TICK_MS: u64 = 50;

pub type DisplayNotifier = Signal<CriticalSectionRawMutex, ViewSpec>;

pub struct Display<'a>(&'a DisplayNotifier);

impl<'a> Display<'a> {
    pub fn new(
        lcd: LcdController,
        notifier: &'static DisplayNotifier,
        spawner: Spawner,
    ) -> Result<Self, SpawnError> {
        spawner.spawn(display_task(lcd, notifier)?);
        Ok(Self(notifier))
    }

    pub fn render(&self, view: ViewSpec) {
        self.0.signal(view);
    }
}

#[embassy_executor::task]
async fn display_task(mut lcd: LcdController, notifier: &'static DisplayNotifier) -> ! {
    let mut current = ViewSpec::Connecting;

    loop {
        match select(
            notifier.wait(),
            Timer::after(Duration::from_millis(ANIM_TICK_MS)),
        )
        .await
        {
            Either::First(view) => current = view,
            Either::Second(_) => {}
        }

        crate::persist::bump_display_heartbeat();
        lcd.render(&current).await;
    }
}
