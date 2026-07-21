//! Layer C: panel-independent, resolution-aware rendering of a [`ViewSpec`]
//! onto any 1-bit [`DrawTarget`].
//!
//! Shared by every *graphical* backend (the Sharp Memory Display today, a
//! same-color-model OLED tomorrow). It knows nothing about a specific panel,
//! its transport, or its flush/VCOM upkeep — only that it has a
//! `DrawTarget<Color = BinaryColor>`.
//!
//! Text uses the large u8g2 bitmap fonts (via [`u8g2_fonts`]) so the readout is
//! legible from across the room. Fonts are chosen by a size tier and the layout
//! is derived from the target's own [`Dimensions`]: transient/prompt screens are
//! vertically centred; the status/recovery/browser screens stack from a top
//! margin. Recipe titles (and any potentially-long text) are word-wrapped to a
//! couple of lines, ellipsising the last line if they still overflow.

use alloc::{format, string::String, vec::Vec};

use embedded_graphics::{pixelcolor::BinaryColor, prelude::*};

use u8g2_fonts::{
    fonts,
    types::{FontColor, HorizontalAlignment, VerticalPosition},
    FontRenderer,
};

use anova_oven_pico_core::fsm::ViewSpec;

use crate::api::celcius_to_fahrenheit;

/// White background (`BinaryColor::Off`), black ink (`BinaryColor::On`) — see
/// the color mapping in `sharp.rs`.
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;

/// Fonts for a given panel size. `hero` is the big headline (temperature),
/// `title` the large title/prompt text, `body` the detail rows.
struct Theme {
    hero: FontRenderer,
    title: FontRenderer,
    body: FontRenderer,
}

/// Pick a font tier from the panel size. The large tier targets the ~400x240
/// Sharp panel (bold FreeUniversal for distance reading); the compact tier
/// keeps a small panel legible. Bump the large-tier fonts (e.g. `fub42`) here to
/// trade characters-per-line for size. `_tf` variants carry the full glyph set
/// (so '…' renders); `ignore_unknown_chars` keeps a rare missing glyph from
/// aborting a draw.
fn theme_for(size: Size) -> Theme {
    if size.width < 200 || size.height < 120 {
        Theme {
            hero: FontRenderer::new::<fonts::u8g2_font_9x15B_tf>().with_ignore_unknown_chars(true),
            title: FontRenderer::new::<fonts::u8g2_font_7x13B_tf>().with_ignore_unknown_chars(true),
            body: FontRenderer::new::<fonts::u8g2_font_6x10_tf>().with_ignore_unknown_chars(true),
        }
    } else {
        Theme {
            hero: FontRenderer::new::<fonts::u8g2_font_fub35_tf>().with_ignore_unknown_chars(true),
            title: FontRenderer::new::<fonts::u8g2_font_fub25_tf>().with_ignore_unknown_chars(true),
            body: FontRenderer::new::<fonts::u8g2_font_helvB18_tf>()
                .with_ignore_unknown_chars(true),
        }
    }
}

fn margin_x(size: Size) -> i32 {
    core::cmp::max(4, (size.width / 32) as i32)
}

fn margin_top(size: Size) -> i32 {
    core::cmp::max(4, (size.height / 24) as i32)
}

fn line_height(font: &FontRenderer) -> i32 {
    font.get_default_line_height() as i32
}

/// Pen-advance width of `s` in `font`, used for word-wrap decisions.
fn text_width(font: &FontRenderer, s: &str) -> u32 {
    font.get_rendered_dimensions(s, Point::zero(), VerticalPosition::Top)
        .map(|d| d.advance.x.max(0) as u32)
        .unwrap_or(0)
}

/// One rendered line: some text in a specific font.
struct Row<'f> {
    font: &'f FontRenderer,
    text: String,
}

fn row(font: &FontRenderer, text: String) -> Row<'_> {
    Row { font, text }
}

/// Greedily word-wrap `text` in `font` to lines no wider than `max_w`, capped at
/// `max_lines` (ellipsising the last kept line when it still overflows).
fn wrap_lines(font: &FontRenderer, text: &str, max_w: u32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let candidate = if cur.is_empty() {
            String::from(word)
        } else {
            format!("{cur} {word}")
        };
        // Always accept the first word of a line so an over-long single word
        // still lands somewhere (it gets ellipsised below if it's the tail).
        if cur.is_empty() || text_width(font, &candidate) <= max_w {
            cur = candidate;
        } else {
            lines.push(core::mem::take(&mut cur));
            cur = String::from(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push('…');
            // Trim content chars (the one before the ellipsis) until it fits.
            while text_width(font, last) > max_w {
                last.pop(); // '…'
                if last.pop().is_none() {
                    last.push('…');
                    break;
                }
                last.push('…');
            }
        }
    }
    lines
}

/// Wrap `text` into a set of [`Row`]s in `font`.
fn title_rows<'f>(font: &'f FontRenderer, text: &str, max_w: u32, max_lines: usize) -> Vec<Row<'f>> {
    wrap_lines(font, text, max_w, max_lines)
        .into_iter()
        .map(|text| Row { font, text })
        .collect()
}

/// Draw one left-aligned line at `(x, *y)` (top-anchored) and advance `*y`.
fn draw_left_line<D>(t: &mut D, font: &FontRenderer, x: i32, y: &mut i32, s: &str)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ = font.render_aligned(
        s,
        Point::new(x, *y),
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(INK),
        t,
    );
    *y += line_height(font);
}

/// Draw `rows` as a vertically-centred, horizontally-centred block.
fn draw_rows_centered<D>(t: &mut D, size: Size, rows: &[Row])
where
    D: DrawTarget<Color = BinaryColor>,
{
    let total: i32 = rows.iter().map(|r| line_height(r.font)).sum();
    let cx = (size.width as i32) / 2;
    let mut y = ((size.height as i32) - total) / 2;
    for r in rows {
        let _ = r.font.render_aligned(
            r.text.as_str(),
            Point::new(cx, y),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(INK),
            t,
        );
        y += line_height(r.font);
    }
}

/// Draw `rows` left-aligned, stacked from the top margin.
fn draw_rows_top<D>(t: &mut D, size: Size, mx: i32, rows: &[Row])
where
    D: DrawTarget<Color = BinaryColor>,
{
    let mut y = margin_top(size);
    for r in rows {
        draw_left_line(t, r.font, mx, &mut y, &r.text);
    }
}

/// Render `view` into `t`, laid out for `t`'s current dimensions.
pub fn render_view<D>(t: &mut D, view: &ViewSpec)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ = t.clear(PAPER);
    let size = t.bounding_box().size;
    let theme = theme_for(size);
    let mx = margin_x(size);
    let content_w = (size.width as i32 - 2 * mx).max(0) as u32;

    match view {
        ViewSpec::WifiInit => {
            draw_rows_centered(t, size, &title_rows(&theme.title, "Connecting to Wi-Fi", content_w, 2))
        }
        ViewSpec::DhcpInit => draw_rows_centered(
            t,
            size,
            &title_rows(&theme.title, "Getting an IP address", content_w, 2),
        ),
        ViewSpec::Connecting => {
            draw_rows_centered(t, size, &title_rows(&theme.title, "Connecting", content_w, 2))
        }
        ViewSpec::ServerOffline => {
            draw_rows_centered(t, size, &title_rows(&theme.title, "Server offline", content_w, 2))
        }
        ViewSpec::UpstreamStale { disconnected_secs } => {
            let mut rows = title_rows(&theme.title, "Oven disconnected", content_w, 2);
            rows.push(row(&theme.body, format!("{disconnected_secs}s ago")));
            draw_rows_centered(t, size, &rows);
        }
        ViewSpec::StartingCook { recipe_title } => {
            let mut rows = Vec::new();
            rows.push(row(&theme.title, String::from("Starting cook")));
            rows.extend(title_rows(&theme.body, recipe_title, content_w, 2));
            draw_rows_centered(t, size, &rows);
        }
        ViewSpec::NextStagePrompt { recipe_title } => {
            let mut rows = Vec::new();
            rows.push(row(&theme.title, String::from("Next stage?")));
            rows.extend(title_rows(&theme.body, recipe_title, content_w, 2));
            draw_rows_centered(t, size, &rows);
        }
        ViewSpec::RecipeBrowser {
            count,
            index,
            title,
        } => {
            if *count == 0 {
                draw_rows_centered(t, size, &[row(&theme.title, String::from("No recipes"))]);
            } else {
                let mut rows = Vec::new();
                rows.push(row(&theme.body, format!("Recipe {}/{}", index + 1, count)));
                rows.extend(title_rows(&theme.title, title, content_w, 3));
                draw_rows_top(t, size, mx, &rows);
            }
        }
        ViewSpec::StopConfirmation { status, cook } => {
            let title = cook
                .as_ref()
                .map(|c| c.display_name())
                .or_else(|| status.as_ref().map(|s| s.phase()))
                .unwrap_or("Active cook");
            let mut rows = title_rows(&theme.title, title, content_w, 2);
            rows.push(row(&theme.title, String::from("Stop cooking?")));
            draw_rows_centered(t, size, &rows);
        }
        ViewSpec::Recovery {
            reset_count,
            panic_count,
            message,
        } => {
            let mut rows = Vec::new();
            rows.push(row(&theme.title, String::from("Recovery")));
            rows.push(row(
                &theme.body,
                format!("resets {reset_count}  panics {panic_count}"),
            ));
            if let Some(msg) = message {
                rows.extend(title_rows(&theme.body, msg, content_w, 2));
            }
            draw_rows_top(t, size, mx, &rows);
        }
        ViewSpec::Status { status, cook } => {
            draw_status(t, size, mx, content_w, &theme, status.as_ref(), cook.as_ref())
        }
    }
}

fn draw_status<D>(
    t: &mut D,
    size: Size,
    mx: i32,
    content_w: u32,
    theme: &Theme,
    status: Option<&anova_oven_api::OvenStatus>,
    cook: Option<&anova_oven_api::CurrentCook>,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let Some(status) = status else {
        draw_rows_centered(t, size, &[row(&theme.title, String::from("Status: N/A"))]);
        return;
    };

    let mut y = margin_top(size);
    let cooking = status.is_cooking();

    // Title: cook name, else manual-cook indicator, else the oven mode.
    let title = if let Some(cook) = cook {
        cook.display_name()
    } else if cooking {
        "Manual cook"
    } else {
        status.mode.as_str()
    };
    for line in wrap_lines(&theme.title, title, content_w, 2) {
        draw_left_line(t, &theme.title, mx, &mut y, &line);
    }

    // Hero temperature: current reading, plus the target while actively
    // cooking. When idle there's no meaningful setpoint, so show current only.
    let cur = celcius_to_fahrenheit(status.current_temperature_c());
    let mut temp = format!("{cur:.0}F");
    if cooking {
        if let Some(target_c) = status.target_temperature_c {
            temp.push_str(&format!(" -> {:.0}F", celcius_to_fahrenheit(target_c)));
        }
    }
    draw_left_line(t, &theme.hero, mx, &mut y, &temp);

    // Detail rows.
    if let Some(remaining) = status.timer_remaining_secs() {
        let (h, m, s) = (remaining / 3600, (remaining % 3600) / 60, remaining % 60);
        let timer = if h > 0 {
            format!("Timer  {h}:{m:02}:{s:02}")
        } else {
            format!("Timer  {m:02}:{s:02}")
        };
        draw_left_line(t, &theme.body, mx, &mut y, &timer);
    }
    if let Some(probe_c) = status.probe_temperature_c {
        draw_left_line(
            t,
            &theme.body,
            mx,
            &mut y,
            &format!("Probe  {:.0}F", celcius_to_fahrenheit(probe_c)),
        );
    }
    if let Some(steam) = status.steam_target_pct {
        draw_left_line(t, &theme.body, mx, &mut y, &format!("Steam  {steam:.0}%"));
    }
    // Phase (Preheating/Cooking) only while cooking — "Phase  Idle" would just
    // repeat the idle title above.
    if cooking {
        draw_left_line(t, &theme.body, mx, &mut y, &format!("Phase  {}", status.phase()));
    }
}
