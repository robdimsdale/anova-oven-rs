//! Layer C renderer: draws a [`ScreenPlan`] onto any 1-bit [`DrawTarget`] using
//! the large u8g2 bitmap fonts, so the readout is legible from across the room.
//!
//! Shared by every *graphical* backend: the 400x240 Sharp Memory Display and
//! the 128x64 SSD1305/SSD1309 OLED. This module owns only *rendering*: choosing
//! fonts by a panel-size tier, measuring text (fed back to the planner so wrap
//! decisions use real glyph widths), dropping lines that don't fit the panel,
//! and placing the rest. The layout *decisions* — which text to show, how to
//! wrap a recipe title, the idle giant-temperature readout, centred vs. top-stacked
//! — live in [`anova_oven_pico_core::view_plan`] so they can be unit-tested on
//! the host.

use embassy_time::Instant;
use embedded_graphics::{pixelcolor::BinaryColor, prelude::*};

use u8g2_fonts::{
    fonts,
    types::{FontColor, HorizontalAlignment, VerticalPosition},
    FontRenderer,
};

use anova_oven_pico_core::fsm::ViewSpec;
use anova_oven_pico_core::view_plan::{
    fit_line_count, plan_view, FontRole, LineMetrics, Placement, PlanLine,
};

/// Background is `BinaryColor::Off`, ink is `BinaryColor::On`. Each driver maps
/// that pair onto its panel's own polarity: black-on-white for the reflective
/// Sharp (`sharp.rs`), lit-on-dark for the emissive OLED (`oled.rs`).
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;

/// Concrete fonts for a panel size. Maps the planner's [`FontRole`]s to u8g2
/// renderers; the same mapping backs the wrap-measurement closure below.
struct Theme {
    giant: FontRenderer,
    /// The unit letter on a [`FontRole::Giant`] line, set a size down on the
    /// digits' own baseline — see [`draw_line`].
    giant_unit: FontRenderer,
    hero: FontRenderer,
    title: FontRenderer,
    body: FontRenderer,
}

impl Theme {
    fn font(&self, role: FontRole) -> &FontRenderer {
        match role {
            FontRole::Giant => &self.giant,
            FontRole::Hero => &self.hero,
            FontRole::Title => &self.title,
            FontRole::Body => &self.body,
        }
    }
}

/// Pick a font tier from the panel size. `_tf` variants carry the full glyph
/// set (so '…' renders); `ignore_unknown_chars` keeps a rare missing glyph from
/// aborting a draw.
///
/// **Every tier is bounded by width, not height**, because the hero line is the
/// one thing the planner never wraps: the widest string it can emit is
/// `"888F -> 888F"`, and a hero font that renders it wider than the content box
/// gets the temperature clipped. That measurement sets each tier:
///
/// | Tier | Panel | Hero | Worst-case hero vs. content |
/// | --- | --- | --- | --- |
/// | compact | 128x64 OLED | 9x18B | 108px vs. 120px |
/// | middle | 320x240 TFT | fub30 | 264px vs. 300px |
/// | large | 400x240 Sharp | fub35 | 320px vs. 380px |
///
/// So `fub35` is *not* usable on the 320-wide TFT even though it has the same
/// height as the Sharp — it needs 320px for a hero the panel has 300px for.
/// Where a bigger hero won't fit, the compact tier buys legibility in height
/// instead (9x18B over 9x15B, same width). Vertical room is the softer
/// constraint: [`fit_line_count`] drops detail rows that don't fit.
///
/// The *giant* role escapes that budget. It only ever carries the idle
/// temperature, and [`draw_line`] splits that into digits and unit, so the
/// digits can use a `_tn` (numerals-only) font — the biggest cut of each
/// family, and the tightest, since a font whose glyphs are all digits has a
/// line box to match. What bounds it is the height of the centred idle block
/// (giant over hero) on the panel:
///
/// | Tier | Giant / unit | Widest giant vs. content | Idle block vs. panel height |
/// | --- | --- | --- | --- |
/// | compact | fub25_tn / t0_11b | 74px vs. 120px | 32 + 19 = 51px vs. 64px |
/// | middle | fub49_tn / fub20 | 155px vs. 300px | 60 + 55 = 115px vs. 240px |
/// | large | fub49_tn / fub25 | 159px vs. 376px | 60 + 66 = 126px vs. 240px |
///
/// `fub49` is where the FreeUniversal Bold family stops; the crate has taller
/// faces (logisoso up to 92) but they are drawn narrow, and a temperature set
/// in one looks stretched next to the rest of the screen. Going meaningfully
/// larger than this means glyphs of our own — see `docs/pico-displays.md`.
/// The unit is paired at about half the digits' cap height.
fn theme_for(size: Size) -> Theme {
    if size.width < 200 || size.height < 120 {
        Theme {
            giant: FontRenderer::new::<fonts::u8g2_font_fub25_tn>().with_ignore_unknown_chars(true),
            giant_unit: FontRenderer::new::<fonts::u8g2_font_t0_11b_tf>()
                .with_ignore_unknown_chars(true),
            hero: FontRenderer::new::<fonts::u8g2_font_9x18B_tf>().with_ignore_unknown_chars(true),
            title: FontRenderer::new::<fonts::u8g2_font_t0_11b_tf>()
                .with_ignore_unknown_chars(true),
            body: FontRenderer::new::<fonts::u8g2_font_5x8_tf>().with_ignore_unknown_chars(true),
        }
    } else if size.width < 360 {
        Theme {
            giant: FontRenderer::new::<fonts::u8g2_font_fub49_tn>().with_ignore_unknown_chars(true),
            giant_unit: FontRenderer::new::<fonts::u8g2_font_fub20_tf>()
                .with_ignore_unknown_chars(true),
            hero: FontRenderer::new::<fonts::u8g2_font_fub30_tf>().with_ignore_unknown_chars(true),
            title: FontRenderer::new::<fonts::u8g2_font_fub20_tf>().with_ignore_unknown_chars(true),
            body: FontRenderer::new::<fonts::u8g2_font_helvB14_tf>()
                .with_ignore_unknown_chars(true),
        }
    } else {
        Theme {
            giant: FontRenderer::new::<fonts::u8g2_font_fub49_tn>().with_ignore_unknown_chars(true),
            giant_unit: FontRenderer::new::<fonts::u8g2_font_fub25_tf>()
                .with_ignore_unknown_chars(true),
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
    core::cmp::max(2, (size.height / 24) as i32)
}

/// Baseline-to-baseline advance: how far the pen drops between lines.
fn line_height(font: &FontRenderer) -> i32 {
    font.get_default_line_height() as i32
}

/// The vertical counterpart of [`text_width`]: real font metrics handed to the
/// planner so it can decide what fits (see [`fit_line_count`]).
fn line_metrics(font: &FontRenderer) -> LineMetrics {
    LineMetrics {
        line_height: line_height(font),
        // Ascender to descender: the tallest ink a line can put down, which is
        // less than the line box because that also counts the empty leading.
        ink_height: (font.get_ascent() as i32 - font.get_descent() as i32).max(0),
    }
}

/// Pen-advance width of `s` in `font`; supplied to the planner for word-wrap.
fn text_width(font: &FontRenderer, s: &str) -> u32 {
    font.get_rendered_dimensions(s, Point::zero(), VerticalPosition::Top)
        .map(|d| d.advance.x.max(0) as u32)
        .unwrap_or(0)
}

/// Split a temperature into the number and its unit: `"212F"` -> `("212",
/// "F")`, `"-40C"` -> `("-40", "C")`. A string with no unit letter is all
/// number, which is also what keeps the digits-only fonts below honest — the
/// number half is exactly what a `_tn` face can render.
fn split_temperature(s: &str) -> (&str, &str) {
    match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => s.split_at(i),
        None => (s, ""),
    }
}

/// Rendered width of a plan line, which for [`FontRole::Giant`] is its two
/// pieces measured in their own fonts. Backs the planner's wrap measurement,
/// so a wrap decision sees the width the line will actually be drawn at.
fn line_width(theme: &Theme, role: FontRole, s: &str) -> u32 {
    if role == FontRole::Giant {
        let (number, unit) = split_temperature(s);
        text_width(&theme.giant, number) + text_width(&theme.giant_unit, unit)
    } else {
        text_width(theme.font(role), s)
    }
}

/// Draw one plan line with its top edge at `y`, placed horizontally about `x`
/// by `align`.
///
/// A [`FontRole::Giant`] line is a temperature, and it is set the way the
/// oven's own panel sets one: the digits large, the unit letter about half
/// that, both sitting on the same baseline so the unit reads as a suffix
/// rather than as a second, smaller word. The pair is placed as one block, so
/// centring centres the whole temperature, not the digits with the unit hung
/// off the side.
fn draw_line<D>(
    t: &mut D,
    theme: &Theme,
    line: &PlanLine,
    x: i32,
    y: i32,
    align: HorizontalAlignment,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    if line.role != FontRole::Giant {
        let _ = theme.font(line.role).render_aligned(
            line.text.as_str(),
            Point::new(x, y),
            VerticalPosition::Top,
            align,
            FontColor::Transparent(INK),
            t,
        );
        return;
    }

    let (number, unit) = split_temperature(&line.text);
    let number_w = text_width(&theme.giant, number) as i32;
    let unit_w = text_width(&theme.giant_unit, unit) as i32;
    let left = match align {
        HorizontalAlignment::Left => x,
        HorizontalAlignment::Center => x - (number_w + unit_w) / 2,
        HorizontalAlignment::Right => x - (number_w + unit_w),
    };
    // Where a `VerticalPosition::Top` draw of the giant font would have put
    // its baseline, so a giant line occupies the same box as any other line.
    let baseline = y + theme.giant.get_ascent() as i32 + 1;

    let _ = theme.giant.render_aligned(
        number,
        Point::new(left, baseline),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(INK),
        t,
    );
    if !unit.is_empty() {
        let _ = theme.giant_unit.render_aligned(
            unit,
            Point::new(left + number_w, baseline),
            VerticalPosition::Baseline,
            HorizontalAlignment::Left,
            FontColor::Transparent(INK),
            t,
        );
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

    // `display_task` re-renders the same `ViewSpec` every animation tick, so
    // the age of its data is the only input that moves between polls — it is
    // what lets the cook timer count in real time (see `plan_view`).
    let age = view.status_age_secs(Instant::now());
    let plan = plan_view(view, content_w, age, |role, s| line_width(&theme, role, s));

    // A plan is sized for its content, not for this panel, so drop the trailing
    // lines that don't fit rather than clipping the last one through the middle
    // of its glyphs. `Centered` measures against the whole panel; `TopStacked`
    // starts at the top margin and so has that much less to work with.
    let avail = match plan.placement {
        Placement::Centered => size.height as i32,
        Placement::TopStacked => size.height as i32 - margin_top(size),
    };
    let kept = fit_line_count(&plan.lines, avail, |role| line_metrics(theme.font(role)));
    let lines = &plan.lines[..kept];

    match plan.placement {
        Placement::Centered => draw_centered(t, size, &theme, lines),
        Placement::TopStacked => draw_top(t, size, mx, &theme, lines),
    }
}

/// Draw the plan as a vertically- and horizontally-centred block.
fn draw_centered<D>(t: &mut D, size: Size, theme: &Theme, lines: &[PlanLine])
where
    D: DrawTarget<Color = BinaryColor>,
{
    let total: i32 = lines.iter().map(|l| line_height(theme.font(l.role))).sum();
    let cx = (size.width as i32) / 2;
    let mut y = ((size.height as i32) - total) / 2;
    for l in lines {
        draw_line(t, theme, l, cx, y, HorizontalAlignment::Center);
        y += line_height(theme.font(l.role));
    }
}

/// Draw the plan left-aligned, stacked from the top margin.
fn draw_top<D>(t: &mut D, size: Size, mx: i32, theme: &Theme, lines: &[PlanLine])
where
    D: DrawTarget<Color = BinaryColor>,
{
    let mut y = margin_top(size);
    for l in lines {
        draw_line(t, theme, l, mx, y, HorizontalAlignment::Left);
        y += line_height(theme.font(l.role));
    }
}
