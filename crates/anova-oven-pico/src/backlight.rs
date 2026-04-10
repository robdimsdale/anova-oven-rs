use embassy_rp::pwm::{Config as PwmConfig, Pwm};

const DEFAULT_FULL_LEVEL: u8 = 255;
const DEFAULT_DIM_LEVEL: u8 = 64;

pub(crate) struct BacklightController {
    pwm_red_green: Pwm<'static>,
    pwm_blue: Pwm<'static>,
    full_level: u8,
    dim_level: u8,
}

impl BacklightController {
    pub(crate) fn new(pwm_red_green: Pwm<'static>, pwm_blue: Pwm<'static>) -> Self {
        Self {
            pwm_red_green,
            pwm_blue,
            full_level: DEFAULT_FULL_LEVEL,
            dim_level: DEFAULT_DIM_LEVEL,
        }
    }

    pub(crate) fn set_full(&mut self) {
        self.set_gray(self.full_level);
    }

    pub(crate) fn set_dim(&mut self) {
        self.set_gray(self.dim_level);
    }

    fn set_gray(&mut self, level: u8) {
        set_backlight_rgb(
            &mut self.pwm_red_green,
            &mut self.pwm_blue,
            level,
            level,
            level,
        );
    }
}

pub fn set_backlight_rgb(pwm_red_green: &mut Pwm<'_>, pwm_blue: &mut Pwm<'_>, r: u8, g: u8, b: u8) {
    let top = 0x8000u16;

    let mut rg_cfg = PwmConfig::default();
    rg_cfg.top = top;
    rg_cfg.invert_a = true;
    rg_cfg.invert_b = true;
    rg_cfg.compare_a = (r as u32 * top as u32 / 255) as u16;
    rg_cfg.compare_b = (g as u32 * top as u32 / 255) as u16;
    pwm_red_green.set_config(&rg_cfg);

    let mut b_cfg = PwmConfig::default();
    b_cfg.top = top;
    b_cfg.invert_a = true;
    b_cfg.compare_a = (b as u32 * top as u32 / 255) as u16;
    pwm_blue.set_config(&b_cfg);
}
