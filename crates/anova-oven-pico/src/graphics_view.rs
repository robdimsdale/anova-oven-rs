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
/// The *giant* role escapes that budget: it only ever carries the idle
/// temperature, whose worst case is `"-888F"`, so it is sized by height —
/// what the centred idle block (giant + hero) can stand on the panel:
///
/// | Tier | Giant | Widest giant vs. content | Idle block vs. panel height |
/// | --- | --- | --- | --- |
/// | compact | fub20 | 70px vs. 120px | 30 + 19 = 49px vs. 64px |
/// | middle | fub42 | 152px vs. 300px | 63 + 55 = 118px vs. 240px |
/// | large | fub42 | 152px vs. 376px | 63 + 66 = 129px vs. 240px |
///
/// `fub42` is the ceiling rather than a height budget: `fub49` ships in a
/// numerals-only charset, which has no `F`. The giant fonts are the `_tr`
/// (ASCII) cut — the role never carries an ellipsis, so the `_tf` glyph set
/// buys nothing, and `_tr`'s tighter default line height makes the idle block
/// shorter into the bargain.
fn theme_for(size: Size) -> Theme {
    if size.width < 200 || size.height < 120 {
        Theme {
            giant: FontRenderer::new::<fonts::u8g2_font_fub20_tr>().with_ignore_unknown_chars(true),
            hero: FontRenderer::new::<fonts::u8g2_font_9x18B_tf>().with_ignore_unknown_chars(true),
            title: FontRenderer::new::<fonts::u8g2_font_t0_11b_tf>()
                .with_ignore_unknown_chars(true),
            body: FontRenderer::new::<fonts::u8g2_font_5x8_tf>().with_ignore_unknown_chars(true),
        }
    } else if size.width < 360 {
        Theme {
            giant: FontRenderer::new::<fonts::u8g2_font_fub42_tr>().with_ignore_unknown_chars(true),
            hero: FontRenderer::new::<fonts::u8g2_font_fub30_tf>().with_ignore_unknown_chars(true),
            title: FontRenderer::new::<fonts::u8g2_font_fub20_tf>().with_ignore_unknown_chars(true),
            body: FontRenderer::new::<fonts::u8g2_font_helvB14_tf>()
                .with_ignore_unknown_chars(true),
        }
    } else {
        Theme {
            giant: FontRenderer::new::<fonts::u8g2_font_fub42_tr>().with_ignore_unknown_chars(true),
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

    let plan = plan_view(view, content_w, |role, s| text_width(theme.font(role), s));

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
        let font = theme.font(l.role);
        let _ = font.render_aligned(
            l.text.as_str(),
            Point::new(cx, y),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(INK),
            t,
        );
        y += line_height(font);
    }
}

/// Draw the plan left-aligned, stacked from the top margin.
fn draw_top<D>(t: &mut D, size: Size, mx: i32, theme: &Theme, lines: &[PlanLine])
where
    D: DrawTarget<Color = BinaryColor>,
{
    let mut y = margin_top(size);
    for l in lines {
        let font = theme.font(l.role);
        let _ = font.render_aligned(
            l.text.as_str(),
            Point::new(mx, y),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            FontColor::Transparent(INK),
            t,
        );
        y += line_height(font);
    }
}
