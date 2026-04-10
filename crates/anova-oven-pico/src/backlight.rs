use defmt::debug;
use embassy_rp::pwm::{Config as PwmConfig, Pwm};

const DEFAULT_FULL_LEVEL: u8 = 255;
const DEFAULT_DIM_LEVEL: u8 = 64;

pub(crate) struct BacklightController {
    pwm_red_green: Pwm<'static>,
    pwm_blue: Pwm<'static>,
    base_config: PwmConfig,
    full_level: u8,
    dim_level: u8,
    // track whether we're currently dimmed to avoid redundant config updates
    backlight_dimmed: bool,
}

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
