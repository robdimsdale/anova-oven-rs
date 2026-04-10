use hd44780_driver::non_blocking::{Cursor, CursorBlink, Display, DisplayMode, HD44780};

use embassy_time::Delay;

pub fn celcius_to_fahrenheit(c: f32) -> f32 {
    c * 1.8 + 32.0
}

pub async fn configure_lcd_display<
    B: hd44780_driver::non_blocking::bus::DataBus,
    M: hd44780_driver::memory_map::DisplayMemoryMap,
    C: hd44780_driver::charset::CharsetWithFallback,
>(
    lcd: &mut HD44780<B, M, C>,
    delay: &mut Delay,
) {
    lcd.set_display_mode(
        DisplayMode {
            cursor_visibility: Cursor::Invisible,
            cursor_blink: CursorBlink::Off,
            display: Display::On,
        },
        delay,
    )
    .await
    .ok();
    lcd.reset(delay).await.ok();
    lcd.clear(delay).await.ok();
}

pub async fn render_status_display<
    B: hd44780_driver::non_blocking::bus::DataBus,
    M: hd44780_driver::memory_map::DisplayMemoryMap,
    C: hd44780_driver::charset::CharsetWithFallback,
>(
    lcd: &mut HD44780<B, M, C>,
    delay: &mut Delay,
    tick: u64,
    status: Option<&anova_oven_api::OvenStatus>,
    current_cook: Option<&anova_oven_api::CurrentCook>,
) {
    const ROTATION_PERIOD: u64 = 3;

    let Some(status) = status else {
        lcd_write_row(lcd, delay, 0, "").await;
        lcd_write_row(lcd, delay, 1, "Status: N/A").await;
        return;
    };

    let is_cooking = current_cook.is_some() && status.is_cooking();

    if is_cooking {
        let cook = current_cook.expect("current cook should exist when is_cooking is true");

        let name = cook.display_name();
        lcd_write_row(lcd, delay, 0, name).await;

        let has_timer_or_probe =
            status.timer_remaining_secs().is_some() || status.probe_temperature_c.is_some();
        let num_items: u64 = if has_timer_or_probe { 4 } else { 3 };
        let slot = (tick / ROTATION_PERIOD) % num_items;

        let phase = status.phase();

        match slot {
            0 => {
                let stage = cook.current_stage(status);
                let row1 = match stage.and_then(|s| s.title.as_deref()) {
                    Some(t) => alloc::string::String::from(t),
                    None if cook.recipe_title == "[custom]" => {
                        alloc::string::String::from("Manual stage")
                    }
                    None => {
                        alloc::format!("Stage: {phase}")
                    }
                };
                lcd_write_row(lcd, delay, 1, &row1).await;
            }
            1 => {
                lcd.set_cursor_xy((0, 1), delay).await.ok();
                let current_f = celcius_to_fahrenheit(status.current_temperature_c());
                let s = alloc::format!("{:.0}", current_f);
                let mut len = s.len() + 2;
                lcd.write_str(&s, delay).await.ok();
                lcd.write_byte(0xDF, delay).await.ok();
                lcd.write_str("F", delay).await.ok();
                if let Some(target_c) = status.target_temperature_c {
                    let target_f = celcius_to_fahrenheit(target_c);
                    let t = alloc::format!(">{:.0}", target_f);
                    len += t.len() + 2;
                    lcd.write_str(&t, delay).await.ok();
                    lcd.write_byte(0xDF, delay).await.ok();
                    lcd.write_str("F", delay).await.ok();
                }
                for _ in len..16 {
                    lcd.write_byte(b' ', delay).await.ok();
                }
            }
            2 => {
                lcd_write_row(lcd, delay, 1, phase).await;
            }
            3 => {
                if let Some(remaining) = status.timer_remaining_secs() {
                    let h = remaining / 3600;
                    let m = (remaining % 3600) / 60;
                    let s = remaining % 60;
                    let row1 = if h > 0 {
                        alloc::format!("Timer: {h}:{m:02}:{s:02}")
                    } else {
                        alloc::format!("Timer: {m:02}:{s:02}")
                    };
                    lcd_write_row(lcd, delay, 1, &row1).await;
                } else if let Some(probe_c) = status.probe_temperature_c {
                    lcd.set_cursor_xy((0, 1), delay).await.ok();
                    let probe_f = celcius_to_fahrenheit(probe_c);
                    let s = alloc::format!("P:{:.0}", probe_f);
                    let mut len = s.len() + 2;
                    lcd.write_str(&s, delay).await.ok();
                    lcd.write_byte(0xDF, delay).await.ok();
                    lcd.write_str("F", delay).await.ok();
                    let stage = cook.current_stage(status);
                    if let Some(target_c) = stage.and_then(|st| st.probe_target_c) {
                        let target_f = celcius_to_fahrenheit(target_c);
                        let t = alloc::format!(">{:.0}", target_f);
                        len += t.len() + 2;
                        lcd.write_str(&t, delay).await.ok();
                        lcd.write_byte(0xDF, delay).await.ok();
                        lcd.write_str("F", delay).await.ok();
                    }
                    for _ in len..16 {
                        lcd.write_byte(b' ', delay).await.ok();
                    }
                } else {
                    lcd_write_row(lcd, delay, 1, "--").await;
                }
            }
            _ => {}
        }
    } else {
        lcd.set_cursor_xy((0, 0), delay).await.ok();
        let temp_str = alloc::format!(
            "{:.0}",
            celcius_to_fahrenheit(status.current_temperature_c())
        );
        let mut row0_len = temp_str.len() + 2;
        lcd.write_str(&temp_str, delay).await.ok();
        lcd.write_byte(0xDF, delay).await.ok();
        lcd.write_str("F", delay).await.ok();
        if let Some(probe_c) = status.probe_temperature_c {
            let probe_str = alloc::format!(" P:{:.0}", celcius_to_fahrenheit(probe_c));
            row0_len += probe_str.len() + 2;
            lcd.write_str(&probe_str, delay).await.ok();
            lcd.write_byte(0xDF, delay).await.ok();
            lcd.write_str("F", delay).await.ok();
        }
        for _ in row0_len..16 {
            lcd.write_byte(b' ', delay).await.ok();
        }

        let row1 = if let Some(steam) = status.steam_target_pct {
            alloc::format!("{} S:{:.0}%", status.mode, steam)
        } else {
            status.mode.clone()
        };
        lcd_write_row(lcd, delay, 1, &row1).await;
    }
}

pub async fn render_recipe_browser<
    B: hd44780_driver::non_blocking::bus::DataBus,
    M: hd44780_driver::memory_map::DisplayMemoryMap,
    C: hd44780_driver::charset::CharsetWithFallback,
>(
    lcd: &mut HD44780<B, M, C>,
    delay: &mut Delay,
    recipes: &[anova_oven_api::Recipe],
    index: usize,
) {
    if recipes.is_empty() {
        lcd_write_row(lcd, delay, 0, "No recipes").await;
        lcd_write_row(lcd, delay, 1, "").await;
        return;
    }

    let header = alloc::format!("Recipe {}/{}", index + 1, recipes.len());
    lcd_write_row(lcd, delay, 0, &header).await;
    lcd_write_row(lcd, delay, 1, &recipes[index].title).await;
}

async fn lcd_write_row<
    B: hd44780_driver::non_blocking::bus::DataBus,
    M: hd44780_driver::memory_map::DisplayMemoryMap,
    C: hd44780_driver::charset::CharsetWithFallback,
>(
    lcd: &mut HD44780<B, M, C>,
    delay: &mut Delay,
    row: u8,
    text: &str,
) {
    lcd.set_cursor_xy((0, row), delay).await.ok();
    let len = text.len().min(16);
    lcd.write_str(&text[..len], delay).await.ok();
    for _ in len..16 {
        lcd.write_byte(b' ', delay).await.ok();
    }
}
