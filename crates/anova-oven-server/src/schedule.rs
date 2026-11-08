//! Cooks set to start later (local mode): "Start in" so many minutes, or
//! "Start at" a time of day.
//!
//! The state machine keeps at most one, since there is one oven, and starts
//! it on a tick once its time comes (see `run_scheduled_start` there). The
//! local store keeps it in `scheduled-start.json`, so it outlasts a restart.
//! This module has what the HTTP and MQTT processors share: the stored
//! form, and turning "Start in" and "Start at" into a time.

use anova_oven_api::Stage;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

/// How far ahead a cook can be set to start. "Start at" a time of day is
/// always within it.
pub const MAX_AHEAD: SignedDuration = SignedDuration::from_hours(24);

/// How long after its time a cook still starts, while the oven is away or
/// the server was down. After that it's dropped, loudly: a cook hours late
/// is worse than none.
pub const GRACE: SignedDuration = SignedDuration::from_mins(10);

/// The longest "Start in", in minutes: [`MAX_AHEAD`].
pub const MAX_MINUTES: u32 = 24 * 60;

/// A cook set to start later.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scheduled {
    pub at: Timestamp,
    #[serde(flatten)]
    pub cook: ScheduledCook,
}

/// What a scheduled cook starts.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScheduledCook {
    /// A saved recipe, looked up when it starts.
    Recipe { recipe_id: String },
    /// Stages given when it was set, already checked.
    Manual { title: String, stages: Vec<Stage> },
}

/// When to start, as a client asks for it.
#[derive(Clone, Debug, PartialEq)]
pub enum When {
    /// "Start in" so many minutes.
    In { minutes: u32 },
    /// "Start at": `HH:MM`, the next time it comes round where the oven
    /// is, or an RFC 3339 timestamp. `time_zone` is the client's own
    /// (IANA), to check `HH:MM` means the same to both.
    At {
        time: String,
        time_zone: Option<String>,
    },
}

/// When `when` is, from `now`. `oven_zone` is where the oven is
/// (`ANOVA_TIMEZONE`, or the system's), which `HH:MM` is read in.
///
/// Fails rather than guess: `HH:MM` with no zone known, or a client whose
/// zone puts `HH:MM` at a different time from the oven's (the server's zone
/// is likely set wrong, and the oven's clock with it).
pub fn resolve(
    when: &When,
    oven_zone: Option<&TimeZone>,
    now: Timestamp,
) -> Result<Timestamp, String> {
    let at = match when {
        When::In { minutes } => {
            if *minutes == 0 || *minutes > MAX_MINUTES {
                return Err(format!("start in 1 to {MAX_MINUTES} minutes"));
            }
            // To the second: nobody needs the microseconds.
            Timestamp::from_second(now.as_second()).unwrap_or(now)
                + SignedDuration::from_mins(i64::from(*minutes))
        }
        When::At { time, time_zone } => match time.parse::<Timestamp>() {
            Ok(at) => at,
            Err(_) => {
                let (hour, minute) = parse_time_of_day(time)?;
                let oven_zone = oven_zone.ok_or(
                    "the server doesn't know its time zone, so it can't tell when that is; set ANOVA_TIMEZONE",
                )?;
                let at = next_time_of_day(hour, minute, oven_zone, now)?;
                if let Some(name) = time_zone.as_deref().filter(|n| !n.is_empty()) {
                    let theirs =
                        TimeZone::get(name).map_err(|_| format!("unknown time zone `{name}`"))?;
                    if next_time_of_day(hour, minute, &theirs, now)? != at {
                        return Err(format!(
                            "{time} in {name} isn't {time} where the oven is ({}); set ANOVA_TIMEZONE to the oven's time zone",
                            oven_zone.iana_name().unwrap_or("the server's time zone"),
                        ));
                    }
                }
                at
            }
        },
    };
    if at <= now {
        return Err("that time has passed".into());
    }
    if at > now + MAX_AHEAD {
        return Err("a cook can only be set to start up to 24 hours ahead".into());
    }
    Ok(at)
}

/// `H:MM` or `HH:MM`, 24-hour.
pub fn parse_time_of_day(text: &str) -> Result<(i8, i8), String> {
    let bad = || format!("`{text}` isn't a time like 16:30");
    let (h, m) = text.trim().split_once(':').ok_or_else(bad)?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return Err(bad());
    }
    let hour: i8 = h.parse().map_err(|_| bad())?;
    let minute: i8 = m.parse().map_err(|_| bad())?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return Err(bad());
    }
    Ok((hour, minute))
}

/// The next `hour:minute` in `zone` after `now`: today's, or tomorrow's if
/// today's has passed. A time skipped by a clock change is taken as the
/// moment after the gap, as jiff does by default.
fn next_time_of_day(
    hour: i8,
    minute: i8,
    zone: &TimeZone,
    now: Timestamp,
) -> Result<Timestamp, String> {
    let time = jiff::civil::time(hour, minute, 0, 0);
    let mut date = now.to_zoned(zone.clone()).date();
    for _ in 0..2 {
        let at = date
            .to_datetime(time)
            .to_zoned(zone.clone())
            .map_err(|e| e.to_string())?
            .timestamp();
        if at > now {
            return Ok(at);
        }
        date = date.tomorrow().map_err(|e| e.to_string())?;
    }
    Err("couldn't work out when that is".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn at(time: &str, time_zone: Option<&str>) -> When {
        When::At {
            time: time.into(),
            time_zone: time_zone.map(Into::into),
        }
    }

    #[test]
    fn start_in_counts_from_now() {
        let now = ts("2026-11-07T12:00:00Z");
        assert_eq!(
            resolve(&When::In { minutes: 25 }, None, now),
            Ok(ts("2026-11-07T12:25:00Z"))
        );
        assert_eq!(
            resolve(
                &When::In { minutes: 25 },
                None,
                ts("2026-11-07T12:00:00.75Z")
            ),
            Ok(ts("2026-11-07T12:25:00Z"))
        );
        assert!(resolve(&When::In { minutes: 0 }, None, now).is_err());
        assert!(resolve(
            &When::In {
                minutes: MAX_MINUTES + 1
            },
            None,
            now
        )
        .is_err());
    }

    #[test]
    fn start_at_is_read_where_the_oven_is() {
        let la = zone("America/Los_Angeles");
        // 12:00Z is 04:00 in LA (PST, -8).
        let now = ts("2026-11-07T12:00:00Z");
        assert_eq!(
            resolve(&at("16:30", None), Some(&la), now),
            Ok(ts("2026-11-08T00:30:00Z"))
        );
        // 03:00 has passed there today, so it's tomorrow's.
        assert_eq!(
            resolve(&at("3:00", None), Some(&la), now),
            Ok(ts("2026-11-08T11:00:00Z"))
        );
    }

    #[test]
    fn start_at_needs_a_zone() {
        let now = ts("2026-11-07T12:00:00Z");
        let err = resolve(&at("16:30", None), None, now).unwrap_err();
        assert!(err.contains("ANOVA_TIMEZONE"), "{err}");
    }

    #[test]
    fn start_at_fails_when_the_clients_zone_disagrees() {
        let la = zone("America/Los_Angeles");
        let now = ts("2026-11-07T12:00:00Z");
        let err = resolve(&at("16:30", Some("Europe/London")), Some(&la), now).unwrap_err();
        assert!(err.contains("America/Los_Angeles"), "{err}");
        // A zone by another name that keeps the same time agrees. Both
        // are canonical, so every tz database has them; aliases such as
        // `US/Pacific` aren't on every system.
        assert!(resolve(&at("16:30", Some("America/Tijuana")), Some(&la), now).is_ok());
        assert!(resolve(&at("16:30", Some("Nowhere/Special")), Some(&la), now).is_err());
    }

    #[test]
    fn start_at_takes_a_timestamp() {
        let now = ts("2026-11-07T12:00:00Z");
        assert_eq!(
            resolve(&at("2026-11-07T16:30:00Z", None), None, now),
            Ok(ts("2026-11-07T16:30:00Z"))
        );
        assert!(resolve(&at("2026-11-07T11:00:00Z", None), None, now).is_err());
        assert!(resolve(&at("2026-11-09T11:00:00Z", None), None, now).is_err());
    }

    #[test]
    fn start_at_across_a_clock_change() {
        let london = zone("Europe/London");
        // Clocks go back at 02:00 BST on 2026-10-25; the day before, 22:00
        // BST. 16:30 the next day is GMT.
        let now = ts("2026-10-24T21:00:00Z");
        assert_eq!(
            resolve(&at("16:30", None), Some(&london), now),
            Ok(ts("2026-10-25T16:30:00Z"))
        );
    }

    #[test]
    fn times_of_day() {
        assert_eq!(parse_time_of_day("16:30"), Ok((16, 30)));
        assert_eq!(parse_time_of_day(" 7:05 "), Ok((7, 5)));
        for bad in ["24:00", "16:60", "16", "16:3", "4:30pm", "", "a:bc"] {
            assert!(parse_time_of_day(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn stored_form() {
        let recipe = Scheduled {
            at: ts("2026-11-07T16:30:00Z"),
            cook: ScheduledCook::Recipe {
                recipe_id: "r1".into(),
            },
        };
        let json = serde_json::to_value(&recipe).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "at": "2026-11-07T16:30:00Z", "recipe_id": "r1" })
        );
        let back: Scheduled = serde_json::from_value(json).unwrap();
        assert!(matches!(back.cook, ScheduledCook::Recipe { ref recipe_id } if recipe_id == "r1"));
        assert_eq!(back.at, recipe.at);

        let manual: Scheduled = serde_json::from_value(serde_json::json!({
            "at": "2026-11-07T16:30:00Z", "title": "Toast", "stages": [],
        }))
        .unwrap();
        assert!(matches!(manual.cook, ScheduledCook::Manual { ref title, .. } if title == "Toast"));
    }
}
