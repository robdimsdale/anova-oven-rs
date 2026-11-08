//! "Start in" and "Start at" (local mode): `cmd/start_in` and
//! `cmd/start_at`, and the values Home Assistant's buttons use for them.
//!
//! A script sends the whole request as JSON (see [`Request`]). A button's
//! `PRESS` starts the selected recipe with the "Start in minutes" and
//! "Start at time" entities' values, which live here for the same reasons
//! as the selected recipe: they survive an HA restart, and setting one
//! doesn't schedule anything until its button is pressed.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::schedule::{parse_time_of_day, When, MAX_MINUTES};

/// The values the buttons use.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub in_minutes: u32,
    /// `HH:MM`, read where the oven is.
    pub at_time: String,
    /// Home Assistant's own zone, sent with `at_time`, to check it agrees
    /// with the oven's.
    pub time_zone: Option<String>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            in_minutes: 30,
            at_time: "18:00".into(),
            time_zone: None,
        }
    }
}

impl Draft {
    /// The `start_later` topic.
    pub fn state(&self) -> Value {
        json!({ "in_minutes": self.in_minutes, "at_time": self.at_time })
    }

    /// `cmd/start_later/in_minutes`: a number, as Home Assistant sends it
    /// (`30` or `30.0`).
    pub fn set_in_minutes(&mut self, payload: &str) -> Result<(), String> {
        match payload.trim().parse::<f64>() {
            Ok(m) if m >= 1.0 && m <= f64::from(MAX_MINUTES) && m.fract() == 0.0 => {
                self.in_minutes = m as u32;
                Ok(())
            }
            _ => Err(format!(
                "start in wants 1 to {MAX_MINUTES} minutes, not `{payload}`"
            )),
        }
    }

    /// `cmd/start_later/at_time`: `{"time": "16:30", "time_zone": "…"}`,
    /// as the text entity's command template sends it, or just `16:30`.
    pub fn set_at_time(&mut self, payload: &str) -> Result<(), String> {
        #[derive(Deserialize)]
        struct AtTime {
            time: String,
            #[serde(default)]
            time_zone: Option<String>,
        }
        let at = serde_json::from_str::<AtTime>(payload).unwrap_or_else(|_| AtTime {
            time: payload.trim().to_string(),
            time_zone: None,
        });
        parse_time_of_day(&at.time)?;
        self.at_time = at.time.trim().to_string();
        self.time_zone = at.time_zone.filter(|z| !z.is_empty());
        Ok(())
    }

    /// When `command` (`start_in` or `start_at`) asks to start: the
    /// request's own time, else this draft's.
    pub fn when(&self, command: &str, request: &Request) -> When {
        if command == "start_in" {
            return When::In {
                minutes: request.minutes.unwrap_or(self.in_minutes),
            };
        }
        match &request.at {
            Some(time) => When::At {
                time: time.clone(),
                time_zone: request.time_zone.clone(),
            },
            None => When::At {
                time: self.at_time.clone(),
                time_zone: self.time_zone.clone(),
            },
        }
    }
}

/// A `cmd/start_in` or `cmd/start_at` request. Anything missing comes from
/// the selected recipe and the [`Draft`]; any payload that isn't JSON (a
/// button's `PRESS`) is all missing.
#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct Request {
    /// The recipe to start.
    #[serde(default)]
    pub recipe_id: Option<String>,
    /// `start_in`: minutes from now.
    #[serde(default)]
    pub minutes: Option<u32>,
    /// `start_at`: `HH:MM` where the oven is, or an RFC 3339 timestamp.
    #[serde(default)]
    pub at: Option<String>,
    /// `start_at`: the sender's IANA zone, to check `at` against the oven's.
    #[serde(default)]
    pub time_zone: Option<String>,
    /// Replace a cook already scheduled, rather than fail.
    #[serde(default)]
    pub replace: bool,
}

impl Request {
    pub fn parse(payload: &str) -> Self {
        serde_json::from_str(payload).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_from_home_assistant() {
        let mut draft = Draft::default();
        draft.set_in_minutes("45.0").unwrap();
        assert_eq!(draft.in_minutes, 45);
        for bad in ["0", "1441", "2.5", "soon"] {
            assert!(draft.set_in_minutes(bad).is_err(), "{bad}");
        }
        assert_eq!(draft.in_minutes, 45);
    }

    #[test]
    fn times_from_home_assistant() {
        let mut draft = Draft::default();
        draft
            .set_at_time(r#"{"time": "16:30", "time_zone": "Europe/London"}"#)
            .unwrap();
        assert_eq!(draft.at_time, "16:30");
        assert_eq!(draft.time_zone.as_deref(), Some("Europe/London"));
        draft.set_at_time("7:05").unwrap();
        assert_eq!(
            (draft.at_time.as_str(), draft.time_zone.as_deref()),
            ("7:05", None)
        );
        assert!(draft.set_at_time("25:00").is_err());
        assert!(draft.set_at_time(r#"{"time": "noon"}"#).is_err());
        assert_eq!(draft.at_time, "7:05");
    }

    #[test]
    fn a_press_uses_the_draft() {
        let draft = Draft {
            in_minutes: 20,
            at_time: "16:30".into(),
            time_zone: Some("Europe/London".into()),
        };
        let press = Request::parse("PRESS");
        assert_eq!(press, Request::default());
        assert_eq!(draft.when("start_in", &press), When::In { minutes: 20 });
        assert_eq!(
            draft.when("start_at", &press),
            When::At {
                time: "16:30".into(),
                time_zone: Some("Europe/London".into()),
            }
        );
    }

    #[test]
    fn a_request_overrides_the_draft() {
        let draft = Draft::default();
        let request = Request::parse(r#"{"recipe_id": "r1", "minutes": 5, "replace": true}"#);
        assert_eq!(request.recipe_id.as_deref(), Some("r1"));
        assert!(request.replace);
        assert_eq!(draft.when("start_in", &request), When::In { minutes: 5 });
        let request = Request::parse(r#"{"at": "2026-11-07T16:30:00Z"}"#);
        assert_eq!(
            draft.when("start_at", &request),
            When::At {
                time: "2026-11-07T16:30:00Z".into(),
                time_zone: None,
            }
        );
    }
}
