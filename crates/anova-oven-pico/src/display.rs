use embassy_executor::{SpawnError, Spawner};
use embassy_futures::select::{select3, Either3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};

pub use anova_oven_pico_core::fsm::{BacklightPolicy, ViewSpec};

use crate::screen::ActiveScreen;

const ANIM_TICK_MS: u64 = 50;

/// The contract `display_task` (and `main`'s bring-up / recovery rendering)
/// depends on, implemented by every display backend. The active backend is a
/// compile-time `cfg` choice (see [`ActiveScreen`]), so dispatch is static —
/// no `dyn`, no allocation, no `Send` bound (the executor is single-threaded).
#[allow(async_fn_in_trait)] // crate-internal, single-threaded executor; never used as `dyn`
pub trait DisplayBackend {
    /// One-time panel initialisation, called once before the first render.
    async fn configure(&mut self);
    /// Render `view` to the panel.
    async fn render(&mut self, view: &ViewSpec);
    /// React to a `BacklightPolicy` change. Only backends with no separate
    /// backlight GPIO need this — today just the OLED, which has to fake
    /// dimming with its own contrast/display-off commands over the same SPI
    /// bus `render` uses (see `oled_ui.rs`). Everything else is driven
    /// directly by `Ctx.backlight` in the main task instead, so the default
    /// is a no-op.
    async fn set_backlight(&mut self, _policy: BacklightPolicy) {}
}

/// No-op backend selected when no `ui-*` feature is enabled: the firmware runs
/// headless (defmt logging only). Keeps `display_task` and `main`'s render
/// calls valid with no physical panel wired. Only compiled in the headless
/// config (see `screen::ActiveScreen`), so the panel builds don't see it as
/// dead code.
#[cfg(not(any(feature = "ui-lcd", feature = "_graphics")))]
#[derive(Default)]
pub struct NullScreen;

#[cfg(not(any(feature = "ui-lcd", feature = "_graphics")))]
impl NullScreen {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(any(feature = "ui-lcd", feature = "_graphics")))]
impl DisplayBackend for NullScreen {
    async fn configure(&mut self) {}
    async fn render(&mut self, _view: &ViewSpec) {}
}

pub type DisplayNotifier = Signal<CriticalSectionRawMutex, ViewSpec>;
pub type BacklightNotifier = Signal<CriticalSectionRawMutex, BacklightPolicy>;

pub struct Display<'a>(&'a DisplayNotifier, &'a BacklightNotifier);

impl<'a> Display<'a> {
    pub fn new(
        screen: ActiveScreen,
        notifier: &'static DisplayNotifier,
        backlight_notifier: &'static BacklightNotifier,
        spawner: Spawner,
    ) -> Result<Self, SpawnError> {
        spawner.spawn(display_task(screen, notifier, backlight_notifier)?);
        Ok(Self(notifier, backlight_notifier))
    }

    pub fn render(&self, view: ViewSpec) {
        self.0.signal(view);
    }

    /// Forward a `BacklightPolicy` change to whichever backend owns the
    /// panel — see `DisplayBackend::set_backlight`.
    pub fn set_backlight(&self, policy: BacklightPolicy) {
        self.1.signal(policy);
    }
}

#[embassy_executor::task]
async fn display_task(
    mut screen: ActiveScreen,
    notifier: &'static DisplayNotifier,
    backlight_notifier: &'static BacklightNotifier,
) -> ! {
    let mut current = ViewSpec::Connecting;

    loop {
        match select3(
            notifier.wait(),
            backlight_notifier.wait(),
            Timer::after(Duration::from_millis(ANIM_TICK_MS)),
        )
        .await
        {
            Either3::First(view) => current = view,
            Either3::Second(policy) => screen.set_backlight(policy).await,
            Either3::Third(_) => {}
        }

        crate::persist::bump_display_heartbeat();
        screen.render(&current).await;
    }
}
