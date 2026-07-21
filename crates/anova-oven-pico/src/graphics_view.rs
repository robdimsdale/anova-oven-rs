//! Layer C: panel-independent, resolution-aware rendering of a [`ViewSpec`]
//! onto any 1-bit [`DrawTarget`].
//!
//! This is shared by every *graphical* backend (the Sharp Memory Display today,
//! a same-color-model OLED tomorrow) — it knows nothing about a specific panel,
//! its transport, or its flush/VCOM upkeep (those live in the Layer A driver and
//! the Layer B backend). It only needs a `DrawTarget<Color = BinaryColor>`.
//!
//! Layout is derived from the target's own [`Dimensions`] rather than hardcoded
//! pixel coordinates: fonts are chosen by a size tier, the left margin scales
//! with width, transient one/two-line screens are vertically centred, and the
//! status/recovery/browser screens stack from a top margin using each line's
//! font metrics. The default (large) tier keeps the current 400x240 fonts, so
//! that panel is visually unchanged.

use alloc::format;

use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_6X10, FONT_7X13, FONT_9X15},
        MonoFont, MonoTextStyle,
    },
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Baseline, Text},
};

use anova_oven_pico_core::fsm::ViewSpec;

use crate::api::celcius_to_fahrenheit;

/// White background (`BinaryColor::Off`), black ink (`BinaryColor::On`) — see
/// the color mapping in `sharp.rs`.
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;

/// Fonts chosen for a given panel size.
struct Theme {
    title: &'static MonoFont<'static>,
    body: &'static MonoFont<'static>,
}

/// Pick a font tier from the panel size. The large tier is tuned for the
/// ~400x240 Sharp panel (and keeps its current fonts, so it looks the same);
/// the compact tier keeps small panels legible. Add further tiers here.
fn theme_for(size: Size) -> Theme {
    if size.width < 200 || size.height < 120 {
        Theme {
            title: &FONT_7X13,
            body: &FONT_6X10,
        }
    } else {
        Theme {
            title: &FONT_10X20,
            body: &FONT_9X15,
        }
    }
}

fn margin_x(size: Size) -> i32 {
    core::cmp::max(4, (size.width / 32) as i32)
}

fn margin_top(size: Size) -> i32 {
    core::cmp::max(4, (size.height / 24) as i32)
}

/// Vertical space one line of `font` occupies, glyph height plus ~1/3 leading.
fn line_advance(font: &MonoFont) -> i32 {
    let h = font.character_size.height as i32;
    h + h / 3
}

/// Draw one top-left-anchored line at `(x, y)` and return the `y` for the next
/// line below it.
fn draw_line<D>(t: &mut D, x: i32, y: i32, s: &str, font: &MonoFont) -> i32
where
    D: DrawTarget<Color = BinaryColor>,
{
    let style = MonoTextStyle::new(font, INK);
    let _ = Text::with_baseline(s, Point::new(x, y), style, Baseline::Top).draw(t);
    y + line_advance(font)
}

/// Draw a block of lines centred vertically, each left-aligned at `mx`. Used for
/// the transient one/two-line screens.
fn draw_centered<D>(t: &mut D, size: Size, mx: i32, lines: &[(&str, &'static MonoFont<'static>)])
where
    D: DrawTarget<Color = BinaryColor>,
{
    const GAP: i32 = 6;
    let total: i32 = lines
        .iter()
        .map(|(_, f)| f.character_size.height as i32)
        .sum::<i32>()
        + GAP * (lines.len() as i32 - 1).max(0);
    let mut y = ((size.height as i32) - total) / 2;
    for (s, font) in lines {
        let style = MonoTextStyle::new(font, INK);
        let _ = Text::with_baseline(s, Point::new(mx, y), style, Baseline::Top).draw(t);
        y += font.character_size.height as i32 + GAP;
    }
}

/// Render `view` into `t`. Clears to the background first, then lays the view
/// out for `t`'s current dimensions.
pub fn render_view<D>(t: &mut D, view: &ViewSpec)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ = t.clear(PAPER);
    let size = t.bounding_box().size;
    let theme = theme_for(size);
    let mx = margin_x(size);

    match view {
        ViewSpec::WifiInit => {
            draw_centered(t, size, mx, &[("Connecting to Wi-Fi...", theme.title)])
        }
        ViewSpec::DhcpInit => {
            draw_centered(t, size, mx, &[("Getting an IP address...", theme.title)])
        }
        ViewSpec::Connecting => draw_centered(t, size, mx, &[("Connecting...", theme.title)]),
        ViewSpec::ServerOffline => draw_centered(t, size, mx, &[("Server offline", theme.title)]),
        ViewSpec::UpstreamStale { disconnected_secs } => {
            let secs = format!("{disconnected_secs}s ago");
            draw_centered(
                t,
                size,
                mx,
                &[("Oven disconnected", theme.title), (secs.as_str(), theme.body)],
            );
        }
        ViewSpec::StartingCook { recipe_title } => {
            draw_centered(
                t,
                size,
                mx,
                &[("Starting cook", theme.title), (recipe_title.as_str(), theme.body)],
            );
        }
        ViewSpec::NextStagePrompt { recipe_title } => {
            draw_centered(
                t,
                size,
                mx,
                &[("Next stage?", theme.title), (recipe_title.as_str(), theme.body)],
            );
        }
        ViewSpec::RecipeBrowser {
            count,
            index,
            title,
        } => {
            if *count == 0 {
                draw_centered(t, size, mx, &[("No recipes", theme.title)]);
            } else {
                let mut y = margin_top(size);
                let header = format!("Recipe {}/{}", index + 1, count);
                y = draw_line(t, mx, y, &header, theme.body);
                draw_line(t, mx, y, title, theme.title);
            }
        }
        ViewSpec::StopConfirmation { status, cook } => {
            let title = cook
                .as_ref()
                .map(|c| c.display_name())
                .or_else(|| status.as_ref().map(|s| s.phase()))
                .unwrap_or("Active cook");
            draw_centered(
                t,
                size,
                mx,
                &[(title, theme.title), ("Stop cooking?", theme.title)],
            );
        }
        ViewSpec::Recovery {
            reset_count,
            panic_count,
            message,
        } => {
            let mut y = margin_top(size);
            y = draw_line(t, mx, y, "Recovery", theme.title);
            let counts = format!("resets {reset_count}  panics {panic_count}");
            y = draw_line(t, mx, y, &counts, theme.body);
            if let Some(msg) = message {
                draw_line(t, mx, y, msg, theme.body);
            }
        }
        ViewSpec::Status { status, cook } => {
            draw_status(t, size, mx, &theme, status.as_ref(), cook.as_ref())
        }
    }
}

fn draw_status<D>(
    t: &mut D,
    size: Size,
    mx: i32,
    theme: &Theme,
    status: Option<&anova_oven_api::OvenStatus>,
    cook: Option<&anova_oven_api::CurrentCook>,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let Some(status) = status else {
        draw_centered(t, size, mx, &[("Status: N/A", theme.title)]);
        return;
    };

    let mut y = margin_top(size);

    // Title: cook name, else manual-cook indicator, else the oven mode.
    let title = if let Some(cook) = cook {
        cook.display_name()
    } else if status.is_cooking() {
        "Manual cook"
    } else {
        status.mode.as_str()
    };
    y = draw_line(t, mx, y, title, theme.title);

    // Headline temperature: current -> target.
    let cur = celcius_to_fahrenheit(status.current_temperature_c());
    let mut temp = format!("{cur:.0}F");
    if let Some(target_c) = status.target_temperature_c {
        temp.push_str(&format!("  ->  {:.0}F", celcius_to_fahrenheit(target_c)));
    }
    y = draw_line(t, mx, y, &temp, theme.title);

    // Detail rows.
    if let Some(remaining) = status.timer_remaining_secs() {
        let (h, m, s) = (remaining / 3600, (remaining % 3600) / 60, remaining % 60);
        let timer = if h > 0 {
            format!("Timer  {h}:{m:02}:{s:02}")
        } else {
            format!("Timer  {m:02}:{s:02}")
        };
        y = draw_line(t, mx, y, &timer, theme.body);
    }
    if let Some(probe_c) = status.probe_temperature_c {
        y = draw_line(
            t,
            mx,
            y,
            &format!("Probe  {:.0}F", celcius_to_fahrenheit(probe_c)),
            theme.body,
        );
    }
    if let Some(steam) = status.steam_target_pct {
        y = draw_line(t, mx, y, &format!("Steam  {steam:.0}%"), theme.body);
    }
    draw_line(t, mx, y, &format!("Phase  {}", status.phase()), theme.body);
}
