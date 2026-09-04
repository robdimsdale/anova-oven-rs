#[cfg(any(feature = "ui-lcd", feature = "ui-tft-basic"))]
use defmt::debug;
#[cfg(any(feature = "ui-lcd", feature = "ui-tft-basic"))]
use embassy_rp::pwm::{Config as PwmConfig, Pwm};

use crate::state::BacklightPolicy;

#[cfg(any(feature = "ui-lcd", feature = "ui-tft-basic"))]
const DEFAULT_FULL_LEVEL: u8 = 255;
#[cfg(any(feature = "ui-lcd", feature = "ui-tft-basic"))]
const DEFAULT_DIM_LEVEL: u8 = 64;

/// Selects the concrete backlight hardware for the active `ui-*` feature —
/// mirrors `screen::ActiveScreen`. The character LCD has a 3-channel RGB
/// backlight LED (GP6/7/8, PWM slices 3+4); the TFT has a single-channel `BL`
/// input (GP21, PWM slice 2); the Sharp panel is reflective (no backlight)
/// and the OLED is self-emissive (no `BL` pin either), so both — along with
/// the headless build — get the no-op controller.
#[cfg(feature = "ui-lcd")]
pub type ActiveBacklight = BacklightController;

#[cfg(feature = "ui-tft-basic")]
pub type ActiveBacklight = PwmBacklightController;

#[cfg(not(any(feature = "ui-lcd", feature = "ui-tft-basic")))]
pub type ActiveBacklight = NullBacklightController;

#[cfg(feature = "ui-lcd")]
pub(crate) struct BacklightController {
    pwm_red_green: Pwm<'static>,
    pwm_blue: Pwm<'static>,
    base_config: PwmConfig,
    full_level: u8,
    dim_level: u8,
    // track whether we're currently dimmed to avoid redundant config updates
    backlight_dimmed: bool,
}

#[cfg(feature = "ui-lcd")]
impl BacklightController {
    pub(crate) fn new(
        pwm_slice3: embassy_rp::Peri<'static, embassy_rp::peripherals::PWM_SLICE3>,
        pin6: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_6>,
        pin7: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_7>,
        pwm_slice4: embassy_rp::Peri<'static, embassy_rp::peripherals::PWM_SLICE4>,
        pin8: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_8>,
    ) -> Self {
        let mut backlight_cfg = PwmConfig::default();

        backlight_cfg.top = 0x8000u16;
        backlight_cfg.invert_a = true;
        backlight_cfg.invert_b = true;
        backlight_cfg.compare_a = 0;
        backlight_cfg.compare_b = 0;

        let pwm_red_green = Pwm::new_output_ab(pwm_slice3, pin6, pin7, backlight_cfg.clone());

        let pwm_blue = Pwm::new_output_a(pwm_slice4, pin8, backlight_cfg.clone());

        let mut s = Self {
            pwm_red_green,
            pwm_blue,
            full_level: DEFAULT_FULL_LEVEL,
            dim_level: DEFAULT_DIM_LEVEL,
            base_config: backlight_cfg,
            backlight_dimmed: false,
        };

        s.set_full();

        s
    }

    pub(crate) fn set_full(&mut self) {
        if self.backlight_dimmed {
            debug!("Backlight was dimmed, setting to full");
            self.backlight_dimmed = false;
        } else {
            debug!("Backlight already full, no action required");
        }
        self.set_gray(self.full_level);
    }

    pub(crate) fn set_dim(&mut self) {
        if !self.backlight_dimmed {
            debug!("Backlight was full, setting to dim");
            self.backlight_dimmed = true;
        } else {
            debug!("Backlight already dimmed, no action required");
        }
        self.set_gray(self.dim_level);
    }

    pub(crate) fn apply(&mut self, policy: BacklightPolicy) {
        match policy {
            BacklightPolicy::Full => self.set_full(),
            BacklightPolicy::Dim => self.set_dim(),
            // The dim timeout is managed by the Idle state handler; this just applies
            // entry intent immediately and keeps hardware control side-effect free.
            BacklightPolicy::FullThenDimAfter(_) => {
                self.set_full();
            }
        }
    }

    fn set_gray(&mut self, level: u8) {
        self.set_backlight_rgb(level, level, level);
    }

    fn set_backlight_rgb(&mut self, r: u8, g: u8, b: u8) {
        let mut rg_cfg = self.base_config.clone();
        rg_cfg.compare_a = (r as u32 * rg_cfg.top as u32 / 255) as u16;
        rg_cfg.compare_b = (g as u32 * rg_cfg.top as u32 / 255) as u16;
        self.pwm_red_green.set_config(&rg_cfg);

        let mut b_cfg = self.base_config.clone();
        b_cfg.compare_a = (b as u32 * b_cfg.top as u32 / 255) as u16;
        self.pwm_blue.set_config(&b_cfg);
    }
}

/// Single-channel PWM backlight for the TFT's `BL` input: GP21, PWM slice 2
/// channel B. The breakout pulls `BL` high on-board (full brightness with
/// nothing driving it), so an un-inverted duty cycle is enough — 0 sinks the
/// line low through that pull-up, 255 drives it fully high.
#[cfg(feature = "ui-tft-basic")]
pub(crate) struct PwmBacklightController {
    pwm: Pwm<'static>,
    base_config: PwmConfig,
    full_level: u8,
    dim_level: u8,
    backlight_dimmed: bool,
}

#[cfg(feature = "ui-tft-basic")]
impl PwmBacklightController {
    pub(crate) fn new(
        pwm_slice2: embassy_rp::Peri<'static, embassy_rp::peripherals::PWM_SLICE2>,
        pin21: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_21>,
    ) -> Self {
        let mut backlight_cfg = PwmConfig::default();
        backlight_cfg.top = 0x8000u16;
        backlight_cfg.compare_b = 0;

        let pwm = Pwm::new_output_b(pwm_slice2, pin21, backlight_cfg.clone());

        let mut s = Self {
            pwm,
            full_level: DEFAULT_FULL_LEVEL,
            dim_level: DEFAULT_DIM_LEVEL,
            base_config: backlight_cfg,
            backlight_dimmed: false,
        };

        s.set_full();

        s
    }

    pub(crate) fn set_full(&mut self) {
        if self.backlight_dimmed {
            debug!("Backlight was dimmed, setting to full");
            self.backlight_dimmed = false;
        } else {
            debug!("Backlight already full, no action required");
        }
        self.set_level(self.full_level);
    }

    pub(crate) fn set_dim(&mut self) {
        if !self.backlight_dimmed {
            debug!("Backlight was full, setting to dim");
            self.backlight_dimmed = true;
        } else {
            debug!("Backlight already dimmed, no action required");
        }
        self.set_level(self.dim_level);
    }

    pub(crate) fn apply(&mut self, policy: BacklightPolicy) {
        match policy {
            BacklightPolicy::Full => self.set_full(),
            BacklightPolicy::Dim => self.set_dim(),
            BacklightPolicy::FullThenDimAfter(_) => {
                self.set_full();
            }
        }
    }

    fn set_level(&mut self, level: u8) {
        let mut cfg = self.base_config.clone();
        cfg.compare_b = (level as u32 * cfg.top as u32 / 255) as u16;
        self.pwm.set_config(&cfg);
    }
}

/// No-op backlight for panels with no controllable backlight: the Sharp
/// Memory Display (reflective, no `BL` input), the OLED (self-emissive, no
/// `BL` pin — see docs/pico-displays.md's OLED "Known gap: burn-in" for the
/// actual mechanism it would need), and the headless build (no panel wired).
#[cfg(not(any(feature = "ui-lcd", feature = "ui-tft-basic")))]
#[derive(Default)]
pub(crate) struct NullBacklightController;

#[cfg(not(any(feature = "ui-lcd", feature = "ui-tft-basic")))]
impl NullBacklightController {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn set_full(&mut self) {}

    pub(crate) fn set_dim(&mut self) {}

    pub(crate) fn apply(&mut self, _policy: BacklightPolicy) {}
}
