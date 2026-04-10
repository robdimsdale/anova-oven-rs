use embassy_rp::pwm::{Config as PwmConfig, Pwm};

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
