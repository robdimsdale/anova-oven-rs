//! Display decision logic for the 16x2 character LCD: turn a [`ViewSpec`] into
//! an [`LcdPlan`] — the pinned top row plus the rows that share the bottom one
//! in rotation — and then resolve that plan against the clock with
//! [`LcdAnimator`]. Pure and host-testable.
//!
//! The graphical panels have their own planner ([`crate::view_plan`]), and the
//! split is deliberate. That one lays text out in two dimensions and *drops*
//! what doesn't fit; this one spends **time** instead of space, because 32
//! character cells can't hold a cooking status at all: the bottom row rotates
//! through its candidates, and any row wider than [`LCD_WIDTH`] marquees. Same
//! kind of decision, opposite degrade strategy — so they stay separate
//! planners over the one shared [`ViewSpec`] (see `docs/pico-displays.md`).
//!
//! What they *do* share is this side of the host/target line. Timing included:
//! [`LcdAnimator::tick`] takes `now` rather than reading the clock itself, so
//! the marquee and the slot rotation — the parts that used to be unreachable
//! from a test because they lived in the firmware bin — are driven by a fake
//! clock in the tests below.
//!
//! The firmware's `lcd.rs` is left with the HD44780 itself: move the cursor,
//! strobe the bytes for whichever *cells* a tick reports as changed. That the
//! unit is a cell and not a row is a real saving rather than a tidiness — the
//! bus costs ~4.1 ms a character, so a repainted row outlasts the 50 ms render
//! tick. See [`CellSpan`] and the timing table in `lcd.rs`.

use alloc::{format, string::String, vec::Vec};

use embassy_time::{Duration, Instant};

use anova_oven_api::{CurrentCook, OvenStatus};

use crate::api::celcius_to_fahrenheit;
use crate::fsm::ViewSpec;
use crate::view_plan::recipe_browser_header;

/// Character cells per row on the panel this plans for.
pub const LCD_WIDTH: usize = 16;

/// How long a bottom row that *fits* holds before the rotation moves on. A row
/// that has to marquee gets its turn measured by the scroll instead — it is
/// done when it has shown its tail (see [`RowAnim::cycle_done`]).
const MIN_SLOT_HOLD: Duration = Duration::from_millis(3000);

/// How often a marquee advances.
const SCROLL_STEP: Duration = Duration::from_millis(350);

/// Cells a marquee jumps per step. More than one because a 350 ms single-cell
/// crawl takes far too long to get through a recipe title.
const CHAR_SCROLL_COUNT: usize = 3;

/// How long a marquee holds still at each end of its run — once before it
/// starts moving, so the opening characters are readable, and again on the
/// tail before it wraps.
const END_PAUSE: Duration = Duration::from_millis(1200);

/// A fully-decided character-LCD screen.
///
/// `row1_slots` is never empty: the bottom row always has something to show.
/// More than one entry means they take turns, in the order given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LcdPlan {
    /// The top row, pinned for as long as this plan is current.
    pub row0: String,
    /// Candidates for the bottom row, most important first.
    pub row1_slots: Vec<String>,
}

/// The common shape: a pinned top row over a single, non-rotating bottom row.
fn rows(row0: impl Into<String>, row1: impl Into<String>) -> LcdPlan {
    LcdPlan {
        row0: row0.into(),
        row1_slots: alloc::vec![row1.into()],
    }
}

/// Decide the [`LcdPlan`] for `view`. `timer_age_secs` is how long ago the
/// view's server data was fetched ([`ViewSpec::timer_age_secs`]), which is what
/// advances the cook timer between polls.
pub fn plan_lcd(view: &ViewSpec, timer_age_secs: u64) -> LcdPlan {
    match view {
        ViewSpec::WifiInit => rows("Anova Oven", "Init: WIFI..."),
        ViewSpec::DhcpInit => rows("Anova Oven", "Init: DHCP..."),
        ViewSpec::Connecting => rows("Anova Oven", "Connecting..."),
        ViewSpec::ServerOffline => rows("Server Offline", "Check backend"),
        ViewSpec::UpstreamStale { disconnected_secs } => rows(
            "Anova Link Down",
            // By the time this view is up, `disconnected_secs` is past the
            // grace window (60s), so the minute count is always >= 1. The row
            // overflows 16 cells and marquees.
            format!("Stale {}m - check", disconnected_secs / 60),
        ),
        ViewSpec::Status { status, cook, .. } => {
            plan_status(status.as_ref(), cook.as_ref(), timer_age_secs)
        }
        ViewSpec::RecipeBrowser {
            source,
            count,
            index,
            title,
        } => {
            if *count == 0 {
                rows("No recipes", "")
            } else {
                rows(
                    recipe_browser_header(*source, *index, *count),
                    title.as_str(),
                )
            }
        }
        ViewSpec::StopConfirmation { status, cook } => {
            let title = cook
                .as_ref()
                .map(|c| c.display_name())
                .or_else(|| status.as_ref().map(|s| s.phase()))
                .unwrap_or("Active cook");
            rows(title, "Stop cooking?")
        }
        ViewSpec::StartingCook { recipe_title } => rows(recipe_title.as_str(), "Starting..."),
        ViewSpec::NextStagePrompt { recipe_title } => {
            let row0 = if recipe_title.is_empty() {
                "Active cook"
            } else {
                recipe_title.as_str()
            };
            rows(row0, "Next stage ready")
        }
        ViewSpec::Recovery {
            reset_count,
            panic_count,
            message,
        } => {
            let label = if *panic_count > 0 { "Panic" } else { "Reset" };
            let row1 = match message.as_deref() {
                Some(msg) if !msg.is_empty() => msg,
                _ => "no msg",
            };
            rows(format!("{label} p={panic_count} r={reset_count}"), row1)
        }
    }
}

fn plan_status(
    status: Option<&OvenStatus>,
    cook: Option<&CurrentCook>,
    timer_age_secs: u64,
) -> LcdPlan {
    let Some(status) = status else {
        return rows("", "Status: N/A");
    };

    if let Some(cook) = cook {
        return plan_cook(status, cook, timer_age_secs);
    }

    if status.is_cooking() {
        // Cooking with no `CurrentCook` behind it: someone started the oven
        // from its own panel. There are no stages to step through, so nothing
        // rotates — the phase, and the steam setpoint it is chasing, is the
        // whole story.
        let row1 = match status.steam_target_pct {
            Some(steam) => format!("{} S:{steam:.0}%", status.phase()),
            None => String::from(status.phase()),
        };
        return rows("Manual cook", row1);
    }

    // Idle. Nothing is running, so there is no setpoint to chase and no cook to
    // name: the top row goes to the temperature the oven is actually sitting at
    // (plus a probe still left in it), and the mode goes below.
    let mut row0 = format!(
        "{:.0}°F",
        celcius_to_fahrenheit(status.current_temperature_c())
    );
    if let Some(probe_c) = status.probe_temperature_c {
        row0.push_str(&format!(" P:{:.0}°F", celcius_to_fahrenheit(probe_c)));
    }
    let row1 = match status.steam_target_pct {
        Some(steam) => format!("{} S:{steam:.0}%", status.mode),
        None => status.mode.clone(),
    };
    rows(row0, row1)
}

/// A cook the server knows about: the name pins the top row, and everything
/// else competes for the bottom one.
fn plan_cook(status: &OvenStatus, cook: &CurrentCook, timer_age_secs: u64) -> LcdPlan {
    // Resolved by index from the server's tracker, not by matching stage kinds:
    // a roast with a sear stage and a rest stage is two `"cook"` stages, and
    // kind-matching would name the first of them for the whole cook. Unresolved
    // (no tracker yet) means no title and no probe target, which the rows below
    // already degrade around.
    let current_stage = status.current_stage(&cook.stages);
    let phase = status.phase();
    let stage_title = current_stage.and_then(|stage| stage.title.as_deref());

    let mut slots = Vec::new();

    match stage_title {
        Some(title) => slots.push(format!("Stage: {title}")),
        // A manual cook has no recipe, so every stage in it is manual and
        // naming one tells the user nothing row 0's "Manual cook" hasn't.
        // Drop the slot rather than spend a rotation on it.
        None if cook.recipe_title == "[manual]" => {}
        None => slots.push(format!("Stage: {phase}")),
    }

    // The temperature is the one slot that is always present.
    let mut temp = format!(
        "{:.0}F",
        celcius_to_fahrenheit(status.current_temperature_c())
    );
    if let Some(target_c) = status.target_temperature_c {
        temp.push_str(&format!(">{:.0}F", celcius_to_fahrenheit(target_c)));
    }
    slots.push(temp);

    // The phase only earns its own slot when the stage title isn't already
    // saying the same word.
    if stage_title.is_some_and(|title| !title.eq_ignore_ascii_case(phase)) {
        slots.push(format!("Phase: {phase}"));
    }

    // One slot for the countdown, or failing that the probe. A cook has at most
    // one of the two worth watching at a time, and the timer is the one that
    // moves, so it wins the slot whenever it is running.
    if let Some(remaining) = status.timer_remaining_secs_after(timer_age_secs) {
        let (h, m, s) = (remaining / 3600, (remaining % 3600) / 60, remaining % 60);
        slots.push(if h > 0 {
            format!("Timer: {h}:{m:02}:{s:02}")
        } else {
            format!("Timer: {m:02}:{s:02}")
        });
    } else if let Some(probe_c) = status.probe_temperature_c {
        let mut probe = format!("P:{:.0}F", celcius_to_fahrenheit(probe_c));
        if let Some(target_c) = current_stage.and_then(|stage| stage.probe_target_c) {
            probe.push_str(&format!(">{:.0}F", celcius_to_fahrenheit(target_c)));
        }
        slots.push(probe);
    }

    LcdPlan {
        row0: String::from(cook.display_name()),
        row1_slots: slots,
    }
}

/// Exactly [`LCD_WIDTH`] cells: cut to fit, then padded with spaces so a row
/// always overwrites whatever the previous one left behind.
///
/// Counted in `char`s, never bytes — the idle top row carries a `'°'`, which is
/// two bytes of UTF-8 and one cell on the panel.
fn pad_to_width(s: &str) -> String {
    let mut out: String = s.chars().take(LCD_WIDTH).collect();
    for _ in out.chars().count()..LCD_WIDTH {
        out.push(' ');
    }
    out
}

/// One row's marquee state. Positions are in characters, not bytes.
#[derive(Clone, Debug)]
struct RowAnim {
    /// The full, unwindowed text this state is animating. Also the identity of
    /// the row: when the text changes, the animation starts over.
    text: String,
    /// How many cells have scrolled off the left edge.
    offset: usize,
    last_step_at: Instant,
    /// Marquee steps are suppressed until this instant — the end pauses.
    pause_until: Instant,
    /// When this text first went up. What [`MIN_SLOT_HOLD`] is measured from.
    shown_at: Instant,
    /// Set once this text has been shown in full: a short row after its hold,
    /// a long one when the marquee has reached its tail.
    cycle_complete: bool,
}

impl RowAnim {
    fn new(text: &str, now: Instant) -> Self {
        Self {
            text: String::from(text),
            offset: 0,
            last_step_at: now,
            // A long row holds at offset 0 for a beat before it starts moving,
            // so its opening characters can be read.
            pause_until: now + END_PAUSE,
            shown_at: now,
            cycle_complete: false,
        }
    }

    /// Whether this text is too wide for the panel and therefore marquees.
    fn scrolls(&self) -> bool {
        self.text.chars().count() > LCD_WIDTH
    }

    /// The window of `text` visible at `now`, and whether the marquee moved to
    /// get there. A row that fits never "moves".
    fn visible_window(&mut self, now: Instant) -> (String, bool) {
        let chars: Vec<char> = self.text.chars().collect();
        let len = chars.len();

        if len <= LCD_WIDTH {
            // Fits, so there is nothing to scroll and the whole "animation" is
            // the hold that gives the next slot its turn.
            self.offset = 0;
            self.last_step_at = now;
            self.pause_until = now;
            if now.duration_since(self.shown_at) >= MIN_SLOT_HOLD {
                self.cycle_complete = true;
            }
            return (self.text.clone(), true);
        }

        let overflow = len - LCD_WIDTH;
        let mut stepped = false;

        // At most one jump per tick: a late tick must not let the marquee
        // "catch up" by several cells at once, which reads as a stutter.
        if now >= self.pause_until && now.duration_since(self.last_step_at) >= SCROLL_STEP {
            self.last_step_at = now;
            stepped = true;
            if self.offset < overflow {
                self.offset = (self.offset + CHAR_SCROLL_COUNT).min(overflow);
                if self.offset >= overflow {
                    // The tail is on screen. Hold it long enough to read, and
                    // mark the row as having had its turn.
                    self.pause_until = now + END_PAUSE;
                    self.cycle_complete = true;
                }
            } else {
                // Wrap round for another pass. This is what keeps a *single*
                // long row (one that shares the bottom row with nothing) moving
                // instead of parking on its tail forever.
                self.offset = 0;
                self.pause_until = now + END_PAUSE;
            }
        }

        (
            chars[self.offset..self.offset + LCD_WIDTH].iter().collect(),
            stepped,
        )
    }

    /// Whether this row has had its full turn — shown in its entirety, and then
    /// held there long enough to actually be read.
    fn cycle_done(&self, now: Instant) -> bool {
        self.cycle_complete && now >= self.pause_until
    }
}

/// A run of cells to write, and the column its first cell sits at.
///
/// Spans cover *changed* cells only, and are never bridged across unchanged
/// ones. On this panel a cursor move costs exactly what one character write
/// costs — both are one full bus transaction, see the timings in `lcd.rs` — so
/// jumping a gap of `n` unchanged cells to save one cursor move breaks even at
/// `n = 1` and loses beyond it. Maximal runs it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellSpan {
    /// Column of the first cell, in `0..LCD_WIDTH`.
    pub col: u8,
    /// The cells to write there, one per `char`.
    pub text: String,
}

/// The runs of `next` that differ from `prev`, left to right.
///
/// Both are fully rendered rows and so the same length. A mismatch could only
/// mean a bug upstream, and is answered with a whole-row rewrite rather than a
/// half-updated row.
fn changed_spans(prev: &str, next: &str) -> Vec<CellSpan> {
    if prev.chars().count() != next.chars().count() {
        return alloc::vec![CellSpan {
            col: 0,
            text: String::from(next),
        }];
    }

    let mut spans: Vec<CellSpan> = Vec::new();
    let mut run: Option<CellSpan> = None;
    for (i, (p, n)) in prev.chars().zip(next.chars()).enumerate() {
        if p != n {
            run.get_or_insert_with(|| CellSpan {
                col: i as u8,
                text: String::new(),
            })
            .text
            .push(n);
        } else if let Some(span) = run.take() {
            spans.push(span);
        }
    }
    spans.extend(run);
    spans
}

/// One physical row: its animation, plus what the panel is currently showing.
#[derive(Default)]
struct RowState {
    anim: Option<RowAnim>,
    /// The exact cells the panel is currently showing, which is what the next
    /// render is diffed against.
    last_rendered: Option<String>,
}

impl RowState {
    /// Advance this row to `text` at `now` and return the runs of cells that
    /// need writing — empty when the panel already shows the right thing.
    fn advance(&mut self, text: &str, now: Instant) -> Vec<CellSpan> {
        let text_changed = self.anim.as_ref().is_none_or(|a| a.text != text);
        if text_changed {
            self.anim = Some(RowAnim::new(text, now));
        }
        let anim = self.anim.as_mut().expect("set directly above");

        let scrolls = anim.scrolls();
        let (visible, stepped) = anim.visible_window(now);

        // A marquee that hasn't stepped has nothing new to show. Checked before
        // rendering so a paused marquee costs nothing at all, not even a diff.
        if scrolls && !text_changed && !stepped {
            return Vec::new();
        }

        let rendered = pad_to_width(&visible);
        let spans = match self.last_rendered.as_deref() {
            Some(prev) => changed_spans(prev, &rendered),
            // Nothing known to be on the panel yet, so send all of it.
            None => alloc::vec![CellSpan {
                col: 0,
                text: rendered.clone(),
            }],
        };
        self.last_rendered = Some(rendered);
        spans
    }

    fn cycle_done(&self, now: Instant) -> bool {
        self.anim.as_ref().is_some_and(|a| a.cycle_done(now))
    }

    /// Drop the animation so the next [`Self::advance`] starts this row over,
    /// whatever text it is given.
    fn restart(&mut self) {
        self.anim = None;
    }
}

/// What to write to the panel this tick: per row, the runs of cells that
/// changed. An empty row is one the panel is already showing correctly.
///
/// Spans rather than whole rows because the bus is slow enough for it to
/// matter: a full 16-cell row is about 70 ms (`lcd.rs` has the arithmetic)
/// against a 50 ms render tick, so a cook timer going `05:00` to `04:59` sends
/// three characters in two spans instead of repainting the row, and a screen
/// that hasn't changed sends nothing at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LcdFrame {
    pub row0: Vec<CellSpan>,
    pub row1: Vec<CellSpan>,
}

impl LcdFrame {
    /// Whether this tick has nothing to write at all.
    pub fn is_empty(&self) -> bool {
        self.row0.is_empty() && self.row1.is_empty()
    }
}

/// Resolves an [`LcdPlan`] against the clock: which slot the bottom row is
/// showing, where each marquee has got to, and what has actually changed since
/// the last tick.
///
/// The caller owns the clock — [`Self::tick`] never reads it — so the whole of
/// this is drivable from a test.
#[derive(Default)]
pub struct LcdAnimator {
    row0: RowState,
    row1: RowState,
    /// Index into the current plan's `row1_slots`.
    slot: usize,
}

impl LcdAnimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `plan` at `now`, returning the rows that need writing.
    pub fn tick(&mut self, plan: &LcdPlan, now: Instant) -> LcdFrame {
        let slots = plan.row1_slots.len().max(1);

        if slots > 1 && self.row1.cycle_done(now) {
            // Rotate *before* choosing, so a slot whose turn has ended doesn't
            // get one more tick on the panel.
            self.slot = (self.slot + 1) % slots;
            // The incoming slot starts its hold and its marquee from scratch,
            // even in the rare case its text matches the outgoing one.
            self.row1.restart();
        } else if self.slot >= slots {
            // The plan lost slots since the last tick — a probe unplugged, a
            // timer run out. Fall back to the last slot that still exists
            // rather than rotating from a stale index.
            self.slot = slots - 1;
            self.row1.restart();
        }

        let row1_text = plan.row1_slots.get(self.slot).map_or("", String::as_str);

        LcdFrame {
            row0: self.row0.advance(&plan.row0, now),
            row1: self.row1.advance(row1_text, now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anova_oven_api::RecipeSource;

    fn t(ms: u64) -> Instant {
        Instant::from_millis(ms)
    }

    /// A fully-populated idle `OvenStatus`. Tests tweak the public fields they
    /// care about. 100C is 212F, which is what most of the expectations below
    /// are written against.
    fn oven(mode: &str) -> OvenStatus {
        OvenStatus {
            mode: String::from(mode),
            temperature_unit: String::from("F"),
            temperature_c: 100.0,
            target_temperature_c: None,
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

    /// A mid-cook oven: `phase()` reads "Cooking" and `stage_kind()` "cook", so
    /// a `"cook"` stage in the fixture below is the one that matches.
    fn cooking_oven() -> OvenStatus {
        let mut s = oven("cook");
        s.timer_mode = String::from("running");
        s.timer_current_secs = 300;
        s.timer_total_secs = 3600;
        // The oven reports which stage it is on; `OvenStatus::current_stage`
        // resolves the fixtures' single-stage lists through it.
        s.active_stage_index = Some(0);
        s
    }

    /// `Stage` has a dozen optional fields and the planner reads two of them,
    /// so the fixtures go through JSON rather than a 20-line struct literal.
    fn cook(recipe_title: &str, stages_json: &str) -> CurrentCook {
        let json = format!(
            r#"{{"recipe_title":"{recipe_title}","started_at":"now",
                 "stages":{stages_json},
                 "cook_stage_count":1,"total_stage_count":1}}"#
        );
        serde_json::from_str(&json).expect("cook fixture must parse")
    }

    const NO_STAGES: &str = "[]";
    const ROAST_STAGE: &str = r#"[{"kind":"cook","temperature_c":200.0,
        "steam_pct":0.0,"fan_speed":100,"title":"Roast"}]"#;

    fn status_view(status: Option<OvenStatus>, cook: Option<CurrentCook>) -> ViewSpec {
        ViewSpec::Status {
            status,
            cook,
            timer_anchor: None,
        }
    }

    // --- plan_lcd: the transient and prompt screens ---

    #[test]
    fn bring_up_screens_name_the_device_over_the_step() {
        // Row 0 is the same on all three: at 16 cells, the brand is the only
        // thing that makes a bare "Connecting..." legible as *this* device.
        for (view, expected) in [
            (ViewSpec::WifiInit, "Init: WIFI..."),
            (ViewSpec::DhcpInit, "Init: DHCP..."),
            (ViewSpec::Connecting, "Connecting..."),
        ] {
            let plan = plan_lcd(&view, 0);
            assert_eq!(plan.row0, "Anova Oven");
            assert_eq!(plan.row1_slots, [expected]);
        }
    }

    #[test]
    fn a_stale_upstream_reports_whole_minutes() {
        let plan = plan_lcd(
            &ViewSpec::UpstreamStale {
                disconnected_secs: 185,
            },
            0,
        );
        assert_eq!(plan.row0, "Anova Link Down");
        assert_eq!(plan.row1_slots, ["Stale 3m - check"]);
    }

    #[test]
    fn a_recovery_screen_distinguishes_a_panic_from_a_reset() {
        let panicked = plan_lcd(
            &ViewSpec::Recovery {
                reset_count: 4,
                panic_count: 2,
                message: Some(String::from("assert failed")),
            },
            0,
        );
        assert_eq!(panicked.row0, "Panic p=2 r=4");
        assert_eq!(panicked.row1_slots, ["assert failed"]);

        // No panics: the same counters, read as a plain reset.
        let reset = plan_lcd(
            &ViewSpec::Recovery {
                reset_count: 4,
                panic_count: 0,
                message: None,
            },
            0,
        );
        assert_eq!(reset.row0, "Reset p=0 r=4");
        // An empty row would read as a hung screen, so say there was no message.
        assert_eq!(reset.row1_slots, ["no msg"]);
    }

    #[test]
    fn an_empty_recovery_message_is_treated_as_no_message() {
        let plan = plan_lcd(
            &ViewSpec::Recovery {
                reset_count: 1,
                panic_count: 0,
                message: Some(String::new()),
            },
            0,
        );
        assert_eq!(plan.row1_slots, ["no msg"]);
    }

    #[test]
    fn the_recipe_browser_pairs_its_header_with_the_title() {
        let plan = plan_lcd(
            &ViewSpec::RecipeBrowser {
                source: RecipeSource::Own,
                count: 3,
                index: 1,
                title: String::from("Roast Chicken"),
            },
            0,
        );
        assert_eq!(plan.row0, "My recipe 2/3");
        assert_eq!(plan.row1_slots, ["Roast Chicken"]);
    }

    #[test]
    fn an_empty_recipe_browser_says_so() {
        let plan = plan_lcd(
            &ViewSpec::RecipeBrowser {
                source: RecipeSource::Bookmarked,
                count: 0,
                index: 0,
                title: String::new(),
            },
            0,
        );
        assert_eq!(plan.row0, "No recipes");
        assert_eq!(plan.row1_slots, [""]);
    }

    #[test]
    fn stop_confirmation_names_the_cook_then_the_phase_then_nothing() {
        let named = plan_lcd(
            &ViewSpec::StopConfirmation {
                status: Some(cooking_oven()),
                cook: Some(cook("Roast Chicken", NO_STAGES)),
            },
            0,
        );
        assert_eq!(named.row0, "Roast Chicken");
        assert_eq!(named.row1_slots, ["Stop cooking?"]);

        // No cook on record, but the oven is running: its phase identifies it.
        let phase_only = plan_lcd(
            &ViewSpec::StopConfirmation {
                status: Some(cooking_oven()),
                cook: None,
            },
            0,
        );
        assert_eq!(phase_only.row0, "Cooking");

        // Nothing at all to go on.
        let bare = plan_lcd(
            &ViewSpec::StopConfirmation {
                status: None,
                cook: None,
            },
            0,
        );
        assert_eq!(bare.row0, "Active cook");
    }

    #[test]
    fn a_next_stage_prompt_falls_back_when_the_cook_is_unnamed() {
        let named = plan_lcd(
            &ViewSpec::NextStagePrompt {
                recipe_title: String::from("Sourdough"),
            },
            0,
        );
        assert_eq!(named.row0, "Sourdough");
        assert_eq!(named.row1_slots, ["Next stage ready"]);

        let unnamed = plan_lcd(
            &ViewSpec::NextStagePrompt {
                recipe_title: String::new(),
            },
            0,
        );
        assert_eq!(unnamed.row0, "Active cook");
    }

    // --- plan_lcd: status, idle ---

    #[test]
    fn a_status_with_no_data_says_so() {
        let plan = plan_lcd(&status_view(None, None), 0);
        assert_eq!(plan.row0, "");
        assert_eq!(plan.row1_slots, ["Status: N/A"]);
    }

    #[test]
    fn an_idle_oven_puts_its_temperature_on_the_top_row() {
        let plan = plan_lcd(&status_view(Some(oven("idle")), None), 0);
        // The degree sign is one cell; `pad_to_width` and the firmware's write
        // path both count characters so the two stay in step.
        assert_eq!(plan.row0, "212°F");
        assert_eq!(plan.row1_slots, ["idle"]);
    }

    #[test]
    fn an_idle_oven_keeps_a_probe_reading_beside_the_temperature() {
        let mut status = oven("idle");
        status.probe_temperature_c = Some(60.0); // 140F
        let plan = plan_lcd(&status_view(Some(status), None), 0);
        // 15 cells, so it still fits without scrolling.
        assert_eq!(plan.row0, "212°F P:140°F");
        assert_eq!(plan.row0.chars().count(), 13);
    }

    #[test]
    fn an_idle_oven_shows_a_steam_setpoint_with_its_mode() {
        let mut status = oven("idle");
        status.steam_target_pct = Some(30.0);
        let plan = plan_lcd(&status_view(Some(status), None), 0);
        assert_eq!(plan.row1_slots, ["idle S:30%"]);
    }

    #[test]
    fn an_idle_oven_never_rotates() {
        let plan = plan_lcd(&status_view(Some(oven("idle")), None), 0);
        assert_eq!(plan.row1_slots.len(), 1);
    }

    // --- plan_lcd: status, cooking ---

    #[test]
    fn a_manual_cook_shows_its_phase_and_steam() {
        let mut status = cooking_oven();
        status.steam_target_pct = Some(80.0);
        // Cooking, but the server has no `CurrentCook` for it.
        let plan = plan_lcd(&status_view(Some(status), None), 0);
        assert_eq!(plan.row0, "Manual cook");
        assert_eq!(plan.row1_slots, ["Cooking S:80%"]);
    }

    #[test]
    fn a_cook_rotates_its_stage_temperature_phase_and_timer() {
        let mut status = cooking_oven();
        status.target_temperature_c = Some(110.0); // 230F
        let plan = plan_lcd(
            &status_view(Some(status), Some(cook("Roast Chicken", ROAST_STAGE))),
            0,
        );

        assert_eq!(plan.row0, "Roast Chicken");
        assert_eq!(
            plan.row1_slots,
            [
                "Stage: Roast",
                "212F>230F",
                "Phase: Cooking",
                // 300s on the clock, read the instant it was fetched.
                "Timer: 05:00",
            ]
        );
    }

    #[test]
    fn a_manual_cook_spends_no_slot_naming_its_stage() {
        // "[manual]" is the server's marker for a cook with no recipe behind
        // it. Every stage in one is manual, so "Stage: Cooking" would only
        // repeat what row 0 already says.
        let plan = plan_lcd(
            &status_view(Some(cooking_oven()), Some(cook("[manual]", NO_STAGES))),
            0,
        );
        assert_eq!(plan.row0, "Manual cook");
        assert_eq!(plan.row1_slots, ["212F", "Timer: 05:00"]);
    }

    #[test]
    fn an_unnamed_stage_of_a_real_recipe_falls_back_to_the_phase() {
        let plan = plan_lcd(
            &status_view(Some(cooking_oven()), Some(cook("Roast Chicken", NO_STAGES))),
            0,
        );
        // No stage title to show, but this *is* a recipe cook, so the slot is
        // still worth spending — it says which part of the recipe is running.
        assert_eq!(plan.row1_slots[0], "Stage: Cooking");
    }

    #[test]
    fn the_stage_is_resolved_by_index_not_by_matching_kinds() {
        // Two `"cook"` stages. Matching on kind would name the first of them
        // for the whole cook, so "Rest" would never reach the panel.
        let stages = r#"[
            {"kind":"preheat","temperature_c":220.0,"steam_pct":0.0,
             "fan_speed":100,"title":"Preheat"},
            {"kind":"cook","temperature_c":200.0,"steam_pct":0.0,
             "fan_speed":100,"title":"Sear"},
            {"kind":"cook","temperature_c":120.0,"steam_pct":0.0,
             "fan_speed":50,"title":"Rest"}
        ]"#;
        let mut status = cooking_oven();
        status.cook_progress = Some(anova_oven_api::CookProgress {
            recipe_title: String::from("Roast Chicken"),
            current_stage_index: 2,
            total_stage_count: 3,
            current_stage_description: String::from("Rest"),
            current_stage_kind: String::from("cook"),
            next_stage_ready: false,
            next_stage_description: None,
        });

        let plan = plan_lcd(
            &status_view(Some(status), Some(cook("Roast Chicken", stages))),
            0,
        );
        assert_eq!(plan.row1_slots[0], "Stage: Rest");
    }

    #[test]
    fn an_unresolvable_stage_shows_no_title_and_no_probe_target() {
        let stage = r#"[{"kind":"cook","temperature_c":200.0,"steam_pct":0.0,
            "fan_speed":100,"title":"Roast","probe_target_c":75.0}]"#;
        let mut status = cooking_oven();
        // Nothing reports an index: no tracker yet, and the oven hasn't said.
        status.active_stage_index = None;
        status.timer_mode = String::from("idle");
        status.timer_total_secs = 0;
        status.probe_temperature_c = Some(60.0);

        let plan = plan_lcd(
            &status_view(Some(status), Some(cook("Roast Chicken", stage))),
            0,
        );
        // The stage slot is still worth spending, it just can't name the
        // stage — and with no stage resolved there is no probe target either.
        // Degraded rows, not wrong ones.
        assert_eq!(plan.row1_slots, ["Stage: Cooking", "212F", "P:140F"]);
    }

    #[test]
    fn the_phase_slot_is_dropped_when_the_stage_title_already_says_it() {
        let stage = r#"[{"kind":"cook","temperature_c":200.0,
            "steam_pct":0.0,"fan_speed":100,"title":"cooking"}]"#;
        let plan = plan_lcd(
            &status_view(Some(cooking_oven()), Some(cook("Roast Chicken", stage))),
            0,
        );
        // Case-insensitively the same word, so a "Phase: Cooking" slot would
        // just be a second turn for "Stage: cooking".
        assert_eq!(plan.row1_slots, ["Stage: cooking", "212F", "Timer: 05:00"]);
    }

    #[test]
    fn the_cook_timer_counts_down_from_the_fetch_not_the_poll() {
        let cooked = cook("Roast Chicken", ROAST_STAGE);
        let at_fetch = plan_lcd(&status_view(Some(cooking_oven()), Some(cooked.clone())), 0);
        let later = plan_lcd(&status_view(Some(cooking_oven()), Some(cooked)), 45);

        assert!(at_fetch.row1_slots.contains(&String::from("Timer: 05:00")));
        // Same server reading, 45s older: the row ticks on its own rather than
        // stalling between polls.
        assert!(later.row1_slots.contains(&String::from("Timer: 04:15")));
    }

    #[test]
    fn a_timer_over_an_hour_gains_an_hours_field() {
        let mut status = cooking_oven();
        status.timer_current_secs = 3725; // 1:02:05
        status.timer_total_secs = 7200;
        let plan = plan_lcd(
            &status_view(Some(status), Some(cook("Brisket", NO_STAGES))),
            0,
        );
        assert!(plan.row1_slots.contains(&String::from("Timer: 1:02:05")));
    }

    #[test]
    fn the_probe_takes_the_slot_when_no_timer_is_running() {
        let stage = r#"[{"kind":"cook","temperature_c":200.0,"steam_pct":0.0,
            "fan_speed":100,"title":"Roast","probe_target_c":75.0}]"#;
        let mut status = cooking_oven();
        status.timer_mode = String::from("idle");
        status.timer_total_secs = 0;
        status.probe_temperature_c = Some(60.0); // 140F, target 75C = 167F

        let plan = plan_lcd(
            &status_view(Some(status), Some(cook("Roast Chicken", stage))),
            0,
        );
        assert_eq!(
            plan.row1_slots,
            ["Stage: Roast", "212F", "Phase: Cooking", "P:140F>167F"]
        );
    }

    #[test]
    fn a_cook_with_neither_timer_nor_probe_drops_that_slot_entirely() {
        let mut status = cooking_oven();
        status.timer_mode = String::from("idle");
        status.timer_total_secs = 0;
        let plan = plan_lcd(
            &status_view(Some(status), Some(cook("Roast Chicken", ROAST_STAGE))),
            0,
        );
        assert_eq!(plan.row1_slots, ["Stage: Roast", "212F", "Phase: Cooking"]);
    }

    // --- LcdAnimator ---

    fn span(col: u8, text: &str) -> CellSpan {
        CellSpan {
            col,
            text: String::from(text),
        }
    }

    /// A stand-in for the panel: applies each frame exactly the way `lcd.rs`
    /// does, so a test can assert both what was *written* (the spans, and what
    /// they cost) and what the panel reads once they land. Keeping the two
    /// separate is the point — a diff that writes fewer cells is only correct
    /// if the row still ends up right.
    struct Panel {
        cells: [[char; LCD_WIDTH]; 2],
        anim: LcdAnimator,
    }

    impl Panel {
        fn new() -> Self {
            Self {
                cells: [[' '; LCD_WIDTH]; 2],
                anim: LcdAnimator::new(),
            }
        }

        /// Tick the animator at `ms`, apply the frame, and hand it back.
        fn tick(&mut self, plan: &LcdPlan, ms: u64) -> LcdFrame {
            let frame = self.anim.tick(plan, t(ms));
            for (row, spans) in [(0usize, &frame.row0), (1usize, &frame.row1)] {
                for span in spans {
                    for (i, ch) in span.text.chars().enumerate() {
                        self.cells[row][span.col as usize + i] = ch;
                    }
                }
            }
            frame
        }

        fn row(&self, row: usize) -> String {
            self.cells[row].iter().collect()
        }
    }

    /// What a frame costs on the bus, in write_byte-equivalents: one cursor
    /// move per span plus one write per cell. See the timing table in `lcd.rs`
    /// — a cursor move and a character cost the same, which is what makes this
    /// a fair single number.
    fn bus_writes(frame: &LcdFrame) -> usize {
        [&frame.row0, &frame.row1]
            .into_iter()
            .flatten()
            .map(|s| 1 + s.text.chars().count())
            .sum()
    }

    // --- LcdAnimator: rows that fit ---

    #[test]
    fn the_first_write_of_a_row_sends_all_of_it() {
        let mut p = Panel::new();
        let frame = p.tick(&rows("Anova Oven", "Connecting..."), 0);

        // Nothing is known to be on the panel yet, so both rows go out whole —
        // padded, not trimmed, since the cells beyond the text have to be
        // cleared too.
        assert_eq!(frame.row0, [span(0, "Anova Oven      ")]);
        assert_eq!(frame.row1, [span(0, "Connecting...   ")]);
        assert_eq!(p.row(0), "Anova Oven      ");
        assert_eq!(p.row(1), "Connecting...   ");
    }

    #[test]
    fn a_degree_sign_costs_one_cell_not_two() {
        let mut p = Panel::new();
        p.tick(&rows("212°F", "idle"), 0);
        // Two bytes of UTF-8, one cell on the panel: 5 characters of text and
        // 11 of padding.
        assert_eq!(p.row(0), "212°F           ");
        assert_eq!(p.row(0).chars().count(), LCD_WIDTH);
    }

    #[test]
    fn a_steady_screen_writes_nothing() {
        let mut p = Panel::new();
        let plan = rows("Anova Oven", "Connecting...");

        assert!(!p.tick(&plan, 0).is_empty());
        // Nothing has changed, so there is nothing to send to the panel.
        assert!(p.tick(&plan, 50).is_empty());
        assert!(p.tick(&plan, 100).is_empty());
        assert_eq!(p.row(1), "Connecting...   ");
    }

    #[test]
    fn a_changed_row_is_rewritten_and_an_unchanged_one_is_not() {
        let mut p = Panel::new();
        p.tick(&rows("Anova Oven", "Init: WIFI..."), 0);

        let frame = p.tick(&rows("Anova Oven", "Init: DHCP..."), 50);
        assert!(frame.row0.is_empty());
        assert_eq!(p.row(1), "Init: DHCP...   ");
    }

    // --- LcdAnimator: writing only the cells that changed ---

    #[test]
    fn only_the_cells_that_changed_are_written() {
        let mut p = Panel::new();
        p.tick(&rows("Roast Chicken", "Timer: 05:00"), 0);

        let frame = p.tick(&rows("Roast Chicken", "Timer: 04:59"), 1000);

        // "Timer: 05:00" -> "Timer: 04:59" differs at cell 8, and at 10-11.
        // Cell 9 (the colon) is untouched and splits the run in two.
        assert_eq!(frame.row1, [span(8, "4"), span(10, "59")]);
        // The pinned title costs nothing.
        assert!(frame.row0.is_empty());
        // Two cursor moves plus three characters, against 17 for the row.
        assert_eq!(bus_writes(&frame), 5);
        // And the row still reads correctly, which is the whole point.
        assert_eq!(p.row(1), "Timer: 04:59    ");
    }

    #[test]
    fn a_one_digit_temperature_change_costs_one_cell() {
        let mut p = Panel::new();
        p.tick(&rows("212°F", "idle"), 0);

        let frame = p.tick(&rows("213°F", "idle"), 1000);
        assert_eq!(frame.row0, [span(2, "3")]);
        // A cursor move and one character: ~8 ms rather than ~70 ms.
        assert_eq!(bus_writes(&frame), 2);
        assert_eq!(p.row(0), "213°F           ");
    }

    #[test]
    fn runs_are_not_bridged_across_unchanged_cells() {
        let mut p = Panel::new();
        p.tick(&rows("AAAAAAAAAAAAAAAA", "x"), 0);

        // Change the first and last cells only. Bridging them would be one
        // span of 16 characters; two spans of one cost 4 writes instead of 17.
        let frame = p.tick(&rows("BAAAAAAAAAAAAAAB", "x"), 1000);
        assert_eq!(frame.row0, [span(0, "B"), span(15, "B")]);
        assert_eq!(bus_writes(&frame), 4);
        assert_eq!(p.row(0), "BAAAAAAAAAAAAAAB");
    }

    #[test]
    fn adjacent_changed_cells_share_one_span() {
        let mut p = Panel::new();
        p.tick(&rows("AAAAAAAAAAAAAAAA", "x"), 0);

        let frame = p.tick(&rows("AAABBBAAAAAAAAAA", "x"), 1000);
        // One seek, three characters — splitting these would pay for two more
        // cursor moves and save nothing.
        assert_eq!(frame.row0, [span(3, "BBB")]);
        assert_eq!(bus_writes(&frame), 4);
    }

    #[test]
    fn a_rotation_writes_only_where_the_two_slots_differ() {
        let mut p = Panel::new();
        let plan = LcdPlan {
            row0: String::from("Roast Chicken"),
            row1_slots: alloc::vec![String::from("Stage: Roast"), String::from("Stage: Rests"),],
        };
        p.tick(&plan, 0);
        p.tick(&plan, 3000);

        // The slots share their "Stage: " prefix, so handing over costs only
        // the cells that actually differ.
        let frame = p.tick(&plan, 3050);
        assert_eq!(frame.row1, [span(8, "ests")]);
        assert_eq!(p.row(1), "Stage: Rests    ");
    }

    #[test]
    fn a_marquee_step_still_rewrites_the_row_it_scrolls() {
        let mut p = Panel::new();
        let plan = rows("Anova Oven", LONG);
        p.tick(&plan, 0);

        // Every cell moves when the window slides, so there is nothing for the
        // diff to save here — it just must not make it *worse*.
        let frame = p.tick(&plan, 1200);
        assert_eq!(frame.row1, [span(0, "w Roasted Pork S")]);
        assert_eq!(bus_writes(&frame), 17);
    }

    // --- LcdAnimator: the marquee ---

    /// 22 characters, so 6 cells of overflow: two 3-cell steps to the tail.
    const LONG: &str = "Slow Roasted Pork Sh..";

    #[test]
    fn a_long_row_holds_still_before_it_starts_scrolling() {
        let mut p = Panel::new();
        let plan = rows("Anova Oven", LONG);

        p.tick(&plan, 0);
        assert_eq!(p.row(1), "Slow Roasted Por");

        // Inside the opening pause: one scroll step's worth of time has passed
        // but the row must not have moved, or the first word is unreadable.
        assert!(p.tick(&plan, 350).row1.is_empty());
        assert!(p.tick(&plan, 1199).row1.is_empty());
    }

    #[test]
    fn a_long_row_marquees_three_cells_at_a_time() {
        let mut p = Panel::new();
        let plan = rows("Anova Oven", LONG);
        p.tick(&plan, 0);

        // First step lands as the opening pause expires.
        p.tick(&plan, 1200);
        assert_eq!(p.row(1), "w Roasted Pork S");
        // Then one step per SCROLL_STEP, and nothing in between.
        assert!(p.tick(&plan, 1400).row1.is_empty());
        // Second step takes it to the tail (6 cells of overflow, 3 per step).
        p.tick(&plan, 1550);
        assert_eq!(p.row(1), "oasted Pork Sh..");
    }

    #[test]
    fn a_long_row_pauses_on_its_tail_then_wraps_to_the_start() {
        let mut p = Panel::new();
        let plan = rows("Anova Oven", LONG);
        p.tick(&plan, 0);
        p.tick(&plan, 1200); // offset 3
        p.tick(&plan, 1550); // offset 6 == overflow, tail reached

        // Held on the tail for the end pause, even though steps are due.
        assert!(p.tick(&plan, 1900).row1.is_empty());
        assert!(p.tick(&plan, 2749).row1.is_empty());
        // Then back to the start for another pass — a single long row has
        // nothing to hand over to, so it loops.
        p.tick(&plan, 2750);
        assert_eq!(p.row(1), "Slow Roasted Por");
    }

    #[test]
    fn a_row_that_changes_mid_scroll_starts_over() {
        let mut p = Panel::new();
        p.tick(&rows("Anova Oven", LONG), 0);
        p.tick(&rows("Anova Oven", LONG), 1200); // scrolled to offset 3

        p.tick(&rows("Anova Oven", "Braised Short Ribs Ext"), 1300);
        // New text, so it reads from its own first character rather than
        // inheriting the previous row's scroll position.
        assert_eq!(p.row(1), "Braised Short Ri");
    }

    // --- LcdAnimator: slot rotation ---

    fn two_slot_plan() -> LcdPlan {
        LcdPlan {
            row0: String::from("Roast Chicken"),
            row1_slots: alloc::vec![String::from("212F>230F"), String::from("Timer: 05:00")],
        }
    }

    #[test]
    fn the_bottom_row_rotates_once_a_slot_has_had_its_turn() {
        let mut p = Panel::new();
        let plan = two_slot_plan();

        p.tick(&plan, 0);
        assert_eq!(p.row(1), "212F>230F       ");
        // Still inside the hold.
        assert!(p.tick(&plan, 2999).row1.is_empty());
        // The hold expires *during* this tick, which is what arms the rotation.
        assert!(p.tick(&plan, 3000).row1.is_empty());
        // So the next tick is the one that swaps the slot.
        p.tick(&plan, 3050);
        assert_eq!(p.row(1), "Timer: 05:00    ");
    }

    #[test]
    fn rotation_comes_back_round_to_the_first_slot() {
        let mut p = Panel::new();
        let plan = two_slot_plan();
        p.tick(&plan, 0);
        p.tick(&plan, 3000);
        p.tick(&plan, 3050); // -> slot 1
        p.tick(&plan, 6050); // slot 1's hold expires

        p.tick(&plan, 6100);
        assert_eq!(p.row(1), "212F>230F       ");
    }

    #[test]
    fn the_top_row_stays_put_while_the_bottom_rotates() {
        let mut p = Panel::new();
        let plan = two_slot_plan();

        assert!(!p.tick(&plan, 0).row0.is_empty());
        p.tick(&plan, 3000);
        let rotated = p.tick(&plan, 3050);

        // The cook name is pinned: rotating the bottom row must not cost a
        // single write on the top one.
        assert!(rotated.row0.is_empty());
        assert!(!rotated.row1.is_empty());
        assert_eq!(p.row(0), "Roast Chicken   ");
    }

    #[test]
    fn rotation_waits_for_a_long_slot_to_finish_scrolling() {
        let plan = LcdPlan {
            row0: String::from("Roast Chicken"),
            row1_slots: alloc::vec![String::from(LONG), String::from("Timer: 05:00")],
        };

        let mut p = Panel::new();
        p.tick(&plan, 0);
        // Past MIN_SLOT_HOLD, but this slot is measured by its marquee: it has
        // not shown its tail yet, so it keeps the row.
        p.tick(&plan, 3050);
        assert_ne!(p.row(1), "Timer: 05:00    ");

        // Tail reached at 1550 and the end pause runs to 2750, so that is the
        // tick that hands the row over.
        let mut p = Panel::new();
        p.tick(&plan, 0);
        p.tick(&plan, 1200); // offset 3
        p.tick(&plan, 1550); // offset 6: tail on screen, turn served
        p.tick(&plan, 2750);
        assert_eq!(p.row(1), "Timer: 05:00    ");
    }

    #[test]
    fn losing_a_slot_clamps_the_rotation_instead_of_reading_past_the_end() {
        let mut p = Panel::new();
        let plan = two_slot_plan();
        p.tick(&plan, 0);
        p.tick(&plan, 3000);
        p.tick(&plan, 3050); // showing slot 1

        // The timer runs out, so the plan comes back a slot shorter.
        p.tick(&rows("Roast Chicken", "212F>230F"), 3100);
        assert_eq!(p.row(1), "212F>230F       ");
    }

    #[test]
    fn a_single_slot_plan_never_rotates() {
        let mut p = Panel::new();
        let plan = rows("Anova Oven", "Connecting...");
        p.tick(&plan, 0);

        // Well past the hold: with nothing to hand over to, the row is simply
        // left alone rather than being rewritten on a phantom rotation.
        for ms in [3000, 3050, 6000, 9000] {
            assert!(p.tick(&plan, ms).is_empty());
        }
        assert_eq!(p.row(1), "Connecting...   ");
    }
}
