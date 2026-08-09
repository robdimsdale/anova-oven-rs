//! Layer C renderer: draws a [`ScreenPlan`] onto any 1-bit [`DrawTarget`] using
//! the large u8g2 bitmap fonts, so the readout is legible from across the room.
//!
//! Shared by every *graphical* backend (the Sharp Memory Display today, a
//! same-color-model OLED tomorrow). This module owns only *rendering*: choosing
//! fonts by a panel-size tier, measuring text (fed back to the planner so wrap
//! decisions use real glyph widths), and placing the plan's lines. The layout
//! *decisions* — which text to show, how to wrap a recipe title, the idle
//! current-only readout, centred vs. top-stacked — live in
//! [`anova_oven_pico_core::view_plan`] so they can be unit-tested on the host.

use embedded_graphics::{pixelcolor::BinaryColor, prelude::*};

use u8g2_fonts::{
    fonts,
    types::{FontColor, HorizontalAlignment, VerticalPosition},
    FontRenderer,
};

use anova_oven_pico_core::fsm::ViewSpec;
use anova_oven_pico_core::view_plan::{plan_view, FontRole, Placement, PlanLine};

/// White background (`BinaryColor::Off`), black ink (`BinaryColor::On`) — see
/// the color mapping in `sharp.rs`.
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;

/// Concrete fonts for a panel size. Maps the planner's [`FontRole`]s to u8g2
/// renderers; the same mapping backs the wrap-measurement closure below.
struct Theme {
    hero: FontRenderer,
    title: FontRenderer,
    body: FontRenderer,
}

impl Theme {
    fn font(&self, role: FontRole) -> &FontRenderer {
        match role {
            FontRole::Hero => &self.hero,
            FontRole::Title => &self.title,
            FontRole::Body => &self.body,
        }
    }
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

    match plan.placement {
        Placement::Centered => draw_centered(t, size, &theme, &plan.lines),
        Placement::TopStacked => draw_top(t, size, mx, &theme, &plan.lines),
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
