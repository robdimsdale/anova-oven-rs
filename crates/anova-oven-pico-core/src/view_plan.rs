//! Display decision logic: turn a [`ViewSpec`] into a renderer-agnostic
//! [`ScreenPlan`] — the lines of text, their font roles, and how the block is
//! placed. Pure and host-testable.
//!
//! The firmware's `graphics_view` maps [`FontRole`]s to concrete fonts, supplies
//! a text-measurement closure (so wrapping uses real glyph widths), and draws
//! the plan. Keeping the *decisions* here (what text to show, when to show a
//! target temperature, how to wrap a long recipe title, what to drop when the
//! panel is too short — see [`fit_line_count`]) means they can be unit tested on
//! the host, unlike the firmware bin which only builds for the MCU.

use alloc::{format, string::String, vec::Vec};

use anova_oven_api::{CurrentCook, OvenStatus};

use crate::api::celcius_to_fahrenheit;
use crate::fsm::ViewSpec;

/// Which font a line uses. The renderer maps these to concrete fonts, and its
/// measurement closure must use the *same* mapping so wrap decisions match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontRole {
    /// Larger than [`FontRole::Hero`], and reserved for the idle temperature:
    /// that line is one short number (`"212F"`, at worst `"-888F"`), never
    /// wrapped and never ellipsised, so it can be sized far past the width
    /// budget the hero line has to keep for `"888F -> 888F"`. Renderers may
    /// back it with a digits-and-uppercase font only — don't put prose in it.
    Giant,
    /// Largest of the general-purpose roles — the glanceable headline
    /// (temperature) on a screen that also carries other rows.
    Hero,
    /// Large — titles and prompts.
    Title,
    /// Smaller — detail rows.
    Body,
}

/// How a plan's lines are placed vertically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Whole block centred (transient/prompt screens).
    Centered,
    /// Stacked from the top margin (status/recovery/browser).
    TopStacked,
}

/// One line of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanLine {
    pub role: FontRole,
    pub text: String,
}

/// A fully-decided screen: the lines to draw and how to place them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenPlan {
    pub placement: Placement,
    pub lines: Vec<PlanLine>,
}

/// Vertical metrics of one font, supplied by the renderer to
/// [`fit_line_count`] the way `measure` supplies horizontal ones to
/// [`plan_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineMetrics {
    /// Baseline-to-baseline advance: how far the pen drops between lines.
    pub line_height: i32,
    /// Tallest ink a line can actually put on the panel, ascender to
    /// descender. At most `line_height`, which also counts the empty leading
    /// below the descender.
    pub ink_height: i32,
}

/// How many leading lines of `lines` fit in `avail` vertical pixels.
///
/// A [`ScreenPlan`] is sized for its *content*, not for any particular panel,
/// so a plan that fits a 400x240 Sharp Memory Display can overflow a 128x64
/// OLED: a cooking status carrying timer, probe, steam and phase rows needs one
/// more row than the OLED has. Rendering it anyway would clip the last row
/// through the middle of its glyphs.
///
/// Trimming from the end degrades gracefully because [`plan_view`] orders a
/// [`Placement::TopStacked`] plan by importance — title, hero temperature, then
/// detail rows in descending relevance — so what drops off is what mattered
/// least.
///
/// Only the *last* kept line is measured by `ink_height`; the ones above it
/// have to leave room for the next line's baseline and so are measured by
/// `line_height`. The trailing line's leading is empty and may hang off the
/// bottom edge without losing a pixel of text. At least one line is always
/// kept, so a plan taller than the panel still renders its headline rather than
/// nothing at all.
pub fn fit_line_count<M>(lines: &[PlanLine], avail: i32, metrics: M) -> usize
where
    M: Fn(FontRole) -> LineMetrics,
{
    let mut used = 0;
    let mut kept = 0;
    for line in lines {
        let m = metrics(line.role);
        if kept > 0 && used + m.ink_height > avail {
            break;
        }
        used += m.line_height;
        kept += 1;
    }
    kept
}

fn planline(role: FontRole, text: String) -> PlanLine {
    PlanLine { role, text }
}

/// Greedily word-wrap `text` in `role`'s font to lines no wider than `max_w`,
/// capped at `max_lines` (ellipsising the last kept line when it still
/// overflows). `measure(role, s)` returns the rendered width of `s`.
fn wrap_lines<M>(
    role: FontRole,
    text: &str,
    max_w: u32,
    max_lines: usize,
    measure: &M,
) -> Vec<String>
where
    M: Fn(FontRole, &str) -> u32,
{
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
        if cur.is_empty() || measure(role, &candidate) <= max_w {
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
            while measure(role, last) > max_w {
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

fn push_wrapped<M>(
    out: &mut Vec<PlanLine>,
    role: FontRole,
    text: &str,
    max_w: u32,
    max_lines: usize,
    measure: &M,
) where
    M: Fn(FontRole, &str) -> u32,
{
    for line in wrap_lines(role, text, max_w, max_lines, measure) {
        out.push(planline(role, line));
    }
}

/// A centred single block wrapping `text` in `role`.
fn centered_wrapped<M>(
    role: FontRole,
    text: &str,
    max_w: u32,
    max_lines: usize,
    measure: &M,
) -> ScreenPlan
where
    M: Fn(FontRole, &str) -> u32,
{
    let mut lines = Vec::new();
    push_wrapped(&mut lines, role, text, max_w, max_lines, measure);
    ScreenPlan {
        placement: Placement::Centered,
        lines,
    }
}

/// Decide the [`ScreenPlan`] for `view`. `content_width` is the usable width
/// (panel width minus margins) used for wrapping; `measure` reports rendered
/// text widths for a given [`FontRole`].
pub fn plan_view<M>(view: &ViewSpec, content_width: u32, measure: M) -> ScreenPlan
where
    M: Fn(FontRole, &str) -> u32,
{
    match view {
        ViewSpec::WifiInit => centered_wrapped(
            FontRole::Title,
            "Connecting to Wi-Fi",
            content_width,
            2,
            &measure,
        ),
        ViewSpec::DhcpInit => centered_wrapped(
            FontRole::Title,
            "Getting an IP address",
            content_width,
            2,
            &measure,
        ),
        ViewSpec::Connecting => {
            centered_wrapped(FontRole::Title, "Connecting", content_width, 2, &measure)
        }
        ViewSpec::ServerOffline => centered_wrapped(
            FontRole::Title,
            "Server offline",
            content_width,
            2,
            &measure,
        ),
        ViewSpec::UpstreamStale { disconnected_secs } => {
            let mut lines = Vec::new();
            push_wrapped(
                &mut lines,
                FontRole::Title,
                "Oven disconnected",
                content_width,
                2,
                &measure,
            );
            lines.push(planline(
                FontRole::Body,
                format!("{disconnected_secs}s ago"),
            ));
            ScreenPlan {
                placement: Placement::Centered,
                lines,
            }
        }
        ViewSpec::StartingCook { recipe_title } => {
            let mut lines = Vec::new();
            lines.push(planline(FontRole::Title, String::from("Starting cook")));
            push_wrapped(
                &mut lines,
                FontRole::Body,
                recipe_title,
                content_width,
                2,
                &measure,
            );
            ScreenPlan {
                placement: Placement::Centered,
                lines,
            }
        }
        ViewSpec::NextStagePrompt { recipe_title } => {
            let mut lines = Vec::new();
            lines.push(planline(FontRole::Title, String::from("Next stage?")));
            push_wrapped(
                &mut lines,
                FontRole::Body,
                recipe_title,
                content_width,
                2,
                &measure,
            );
            ScreenPlan {
                placement: Placement::Centered,
                lines,
            }
        }
        ViewSpec::RecipeBrowser {
            count,
            index,
            title,
        } => {
            if *count == 0 {
                ScreenPlan {
                    placement: Placement::Centered,
                    lines: alloc::vec![planline(FontRole::Title, String::from("No recipes"))],
                }
            } else {
                let mut lines = Vec::new();
                lines.push(planline(
                    FontRole::Body,
                    format!("Recipe {}/{}", index + 1, count),
                ));
                push_wrapped(
                    &mut lines,
                    FontRole::Title,
                    title,
                    content_width,
                    3,
                    &measure,
                );
                ScreenPlan {
                    placement: Placement::TopStacked,
                    lines,
                }
            }
        }
        ViewSpec::StopConfirmation { status, cook } => {
            let title = cook
                .as_ref()
                .map(|c| c.display_name())
                .or_else(|| status.as_ref().map(|s| s.phase()))
                .unwrap_or("Active cook");
            let mut lines = Vec::new();
            push_wrapped(
                &mut lines,
                FontRole::Title,
                title,
                content_width,
                2,
                &measure,
            );
            lines.push(planline(FontRole::Title, String::from("Stop cooking?")));
            ScreenPlan {
                placement: Placement::Centered,
                lines,
            }
        }
        ViewSpec::Recovery {
            reset_count,
            panic_count,
            message,
        } => {
            let mut lines = Vec::new();
            lines.push(planline(FontRole::Title, String::from("Recovery")));
            lines.push(planline(
                FontRole::Body,
                format!("resets {reset_count}  panics {panic_count}"),
            ));
            if let Some(msg) = message {
                push_wrapped(&mut lines, FontRole::Body, msg, content_width, 2, &measure);
            }
            ScreenPlan {
                placement: Placement::TopStacked,
                lines,
            }
        }
        ViewSpec::Status { status, cook } => {
            plan_status(status.as_ref(), cook.as_ref(), content_width, &measure)
        }
    }
}

fn plan_status<M>(
    status: Option<&OvenStatus>,
    cook: Option<&CurrentCook>,
    content_width: u32,
    measure: &M,
) -> ScreenPlan
where
    M: Fn(FontRole, &str) -> u32,
{
    let Some(status) = status else {
        return ScreenPlan {
            placement: Placement::Centered,
            lines: alloc::vec![planline(FontRole::Title, String::from("Status: N/A"))],
        };
    };

    let cooking = status.is_cooking();
    let cur = celcius_to_fahrenheit(status.current_temperature_c());

    // Idle is a glance screen, not a readout: nothing is running, so there is
    // no cook to name and no setpoint to chase. Give the whole panel to the
    // temperature — [`FontRole::Giant`], centred — with the mode word under
    // it, and let any detail row the oven still reports follow below.
    if !cooking {
        let mut lines = alloc::vec![
            planline(FontRole::Giant, format!("{cur:.0}F")),
            planline(FontRole::Hero, String::from(status.phase())),
        ];
        push_detail_rows(&mut lines, status, cooking);
        return ScreenPlan {
            placement: Placement::Centered,
            lines,
        };
    }

    let mut lines = Vec::new();

    // Title: cook name, else manual-cook indicator, else the oven mode.
    let title = if let Some(cook) = cook {
        cook.display_name()
    } else if cooking {
        "Manual cook"
    } else {
        status.mode.as_str()
    };
    push_wrapped(
        &mut lines,
        FontRole::Title,
        title,
        content_width,
        2,
        measure,
    );

    // Hero temperature: current reading and the target we're heating towards.
    let mut temp = format!("{cur:.0}F");
    if let Some(target_c) = status.target_temperature_c {
        temp.push_str(&format!(" -> {:.0}F", celcius_to_fahrenheit(target_c)));
    }
    lines.push(planline(FontRole::Hero, temp));

    push_detail_rows(&mut lines, status, cooking);

    ScreenPlan {
        placement: Placement::TopStacked,
        lines,
    }
}

/// The rows below the headline, in descending importance — `fit_line_count`
/// trims from the end, so the order is what a short panel drops first.
fn push_detail_rows(lines: &mut Vec<PlanLine>, status: &OvenStatus, cooking: bool) {
    if let Some(remaining) = status.timer_remaining_secs() {
        let (h, m, s) = (remaining / 3600, (remaining % 3600) / 60, remaining % 60);
        let timer = if h > 0 {
            format!("Timer  {h}:{m:02}:{s:02}")
        } else {
            format!("Timer  {m:02}:{s:02}")
        };
        lines.push(planline(FontRole::Body, timer));
    }
    if let Some(probe_c) = status.probe_temperature_c {
        lines.push(planline(
            FontRole::Body,
            format!("Probe  {:.0}F", celcius_to_fahrenheit(probe_c)),
        ));
    }
    if let Some(steam) = status.steam_target_pct {
        lines.push(planline(FontRole::Body, format!("Steam  {steam:.0}%")));
    }
    // Phase (Preheating/Cooking) only while cooking — "Phase  Idle" would just
    // repeat the idle title above.
    if cooking {
        lines.push(planline(
            FontRole::Body,
            format!("Phase  {}", status.phase()),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake measurer: every glyph is 10px wide. Deterministic, so wrap points
    /// are exact — `content_width = 100` fits 10 characters.
    fn measure(_role: FontRole, s: &str) -> u32 {
        s.chars().count() as u32 * 10
    }

    fn pl(role: FontRole, text: &str) -> PlanLine {
        PlanLine {
            role,
            text: String::from(text),
        }
    }

    /// A fully-populated idle `OvenStatus` (target set, so tests can prove the
    /// idle path *omits* it). Tests tweak the public fields they care about.
    fn oven(mode: &str) -> OvenStatus {
        OvenStatus {
            mode: String::from(mode),
            temperature_unit: String::from("F"),
            temperature_c: 100.0,              // 212F
            target_temperature_c: Some(180.0), // 356F
            temperature_bulbs_mode: String::from("dry"),
            dry_top_temperature_c: 100.0,
            dry_bottom_temperature_c: 100.0,
            wet_bulb_temperature_c: 100.0,
            probe_temperature_c: None,
            timer_current_secs: 0,
            timer_total_secs: 0,
            timer_mode: String::from("idle"),
            steam_pct: 0.0,
            steam_target_pct: None,
            steam_generator_mode: String::from("idle"),
            boiler_celsius: 0.0,
            boiler_watts: 0.0,
            boiler_descale_required: false,
            evaporator_celsius: 0.0,
            evaporator_watts: 0.0,
            fan_speed: 0,
            heating_element_top_on: false,
            heating_element_top_watts: 0.0,
            heating_element_rear_on: false,
            heating_element_rear_watts: 0.0,
            heating_element_bottom_on: false,
            heating_element_bottom_watts: 0.0,
            lamp_on: false,
            lamp_preference: String::from("off"),
            vent_open: false,
            door_open: false,
            water_tank_empty: false,
            active_stage_index: None,
            active_stage_id: None,
            cook_progress: None,
            upstream: None,
        }
    }

    #[test]
    fn idle_status_is_a_centered_giant_temperature_over_the_mode() {
        let view = ViewSpec::Status {
            status: Some(oven("idle")),
            cook: None,
        };
        let plan = plan_view(&view, 300, measure);

        // The whole panel goes to the temperature: giant and centred, with
        // "Idle" under it. No setpoint on the temperature line (there is none
        // when nothing is running) and no title row repeating the mode.
        assert_eq!(plan.placement, Placement::Centered);
        assert_eq!(
            plan.lines,
            [pl(FontRole::Giant, "212F"), pl(FontRole::Hero, "Idle")]
        );
    }

    #[test]
    fn idle_status_keeps_a_probe_reading_below_the_mode() {
        let mut status = oven("idle");
        status.probe_temperature_c = Some(60.0); // 140F
        let plan = plan_view(
            &ViewSpec::Status {
                status: Some(status),
                cook: None,
            },
            300,
            measure,
        );

        // A probe left in the oven still reads while idle, so it keeps its row.
        assert_eq!(
            plan.lines,
            [
                pl(FontRole::Giant, "212F"),
                pl(FontRole::Hero, "Idle"),
                pl(FontRole::Body, "Probe  140F"),
            ]
        );
    }

    #[test]
    fn cooking_status_shows_target_and_phase() {
        let view = ViewSpec::Status {
            status: Some(oven("cook")),
            cook: None,
        };
        let plan = plan_view(&view, 300, measure);

        // Manual cook (no CurrentCook), hero shows current -> target, and a
        // phase row is present (Preheating: timer idle, no elapsed time).
        assert_eq!(
            plan.lines,
            [
                pl(FontRole::Title, "Manual cook"),
                pl(FontRole::Hero, "212F -> 356F"),
                pl(FontRole::Body, "Phase  Preheating"),
            ]
        );
    }

    #[test]
    fn wrap_splits_on_word_boundaries() {
        // content 100 => 10 chars per line. "aaaa bbbb" = 9 chars fits; adding
        // " cccc" (14) overflows, so it breaks.
        let lines = wrap_lines(FontRole::Title, "aaaa bbbb cccc", 100, 3, &measure);
        assert_eq!(lines, ["aaaa bbbb", "cccc"]);
    }

    #[test]
    fn wrap_ellipsizes_last_line_when_over_max_lines() {
        // Natural wrap is 3 lines ("aa bb" / "cc dd" / "ee") at width 50 (5
        // chars); capped at 2, the last line is ellipsised to fit.
        let lines = wrap_lines(FontRole::Title, "aa bb cc dd ee", 50, 2, &measure);
        assert_eq!(lines, ["aa bb", "cc d…"]);
    }

    /// Fake metrics: Giant 30/24, Hero 20/16, Title 12/10, Body 10/8
    /// (line/ink), so a trailing line's leading is allowed to overhang.
    fn metrics(role: FontRole) -> LineMetrics {
        match role {
            FontRole::Giant => LineMetrics {
                line_height: 30,
                ink_height: 24,
            },
            FontRole::Hero => LineMetrics {
                line_height: 20,
                ink_height: 16,
            },
            FontRole::Title => LineMetrics {
                line_height: 12,
                ink_height: 10,
            },
            FontRole::Body => LineMetrics {
                line_height: 10,
                ink_height: 8,
            },
        }
    }

    #[test]
    fn fit_keeps_everything_that_fits() {
        let lines = [
            pl(FontRole::Title, "Manual cook"),
            pl(FontRole::Hero, "212F"),
            pl(FontRole::Body, "Timer  05:00"),
        ];
        // 12 + 20 + 8(ink) = 40.
        assert_eq!(fit_line_count(&lines, 40, metrics), 3);
    }

    #[test]
    fn fit_drops_trailing_lines_that_overflow() {
        let lines = [
            pl(FontRole::Title, "Manual cook"),
            pl(FontRole::Hero, "212F"),
            pl(FontRole::Body, "Timer  05:00"),
            pl(FontRole::Body, "Probe  130F"),
        ];
        // A fourth row would need 12 + 20 + 10 + 8 = 50.
        assert_eq!(fit_line_count(&lines, 49, metrics), 3);
        assert_eq!(fit_line_count(&lines, 50, metrics), 4);
    }

    #[test]
    fn fit_measures_the_last_line_by_ink_not_line_height() {
        let lines = [pl(FontRole::Title, "Recovery"), pl(FontRole::Body, "x")];
        // Two full line boxes are 22px, but the body line's ink is only 8, so
        // it still fits in 20 with its leading hanging off the bottom.
        assert_eq!(fit_line_count(&lines, 20, metrics), 2);
        assert_eq!(fit_line_count(&lines, 19, metrics), 1);
    }

    #[test]
    fn fit_always_keeps_the_headline() {
        let lines = [pl(FontRole::Hero, "212F"), pl(FontRole::Body, "Steam  60%")];
        // Nothing fits in a zero-height panel; the first line is kept anyway so
        // the render isn't blank.
        assert_eq!(fit_line_count(&lines, 0, metrics), 1);
    }

    #[test]
    fn fit_of_nothing_is_nothing() {
        assert_eq!(fit_line_count(&[], 100, metrics), 0);
    }

    #[test]
    fn transient_screen_is_centered() {
        let plan = plan_view(&ViewSpec::WifiInit, 300, measure);
        assert_eq!(plan.placement, Placement::Centered);
        assert_eq!(plan.lines, [pl(FontRole::Title, "Connecting to Wi-Fi")]);
    }

    #[test]
    fn recipe_browser_empty_vs_populated() {
        let empty = plan_view(
            &ViewSpec::RecipeBrowser {
                count: 0,
                index: 0,
                title: String::new(),
            },
            300,
            measure,
        );
        assert_eq!(empty.placement, Placement::Centered);
        assert_eq!(empty.lines, [pl(FontRole::Title, "No recipes")]);

        let populated = plan_view(
            &ViewSpec::RecipeBrowser {
                count: 3,
                index: 0,
                title: String::from("Cake"),
            },
            300,
            measure,
        );
        assert_eq!(populated.placement, Placement::TopStacked);
        assert_eq!(
            populated.lines,
            [
                pl(FontRole::Body, "Recipe 1/3"),
                pl(FontRole::Title, "Cake")
            ]
        );
    }
}
