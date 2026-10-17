//! Prometheus text rendering of the persist [`Snapshot`] for the
//! firmware's `/metrics` endpoint. See `docs/monitoring.md`, Phase 2.
//!
//! Same data as `/health`, different shape: `/health` is JSON for a
//! human with `curl`, `/metrics` is the exposition format for a scraper.
//! The rendering lives here rather than in the bin so it is
//! host-testable, and so the worst-case size bound below is checked by
//! `cargo test` rather than discovered on the device.
//!
//! Rules this module holds, deliberately:
//!
//! * **No allocation.** The output is a fixed-capacity
//!   [`heapless::String`], so a scrape never adds heap pressure in the
//!   situation it exists to observe.
//! * **No timestamps.** The RP2040 has no wall clock; a boot-relative
//!   timestamp would be dated 1970. The scraper assigns its own, and
//!   `pico_uptime_seconds` carries the device's notion of time as data.
//! * **Closed label sets.** Every label value comes from a closed enum
//!   (`ResetReason::name`, `AppStateLabel::name`, the three heartbeat
//!   tasks), so the series count is fixed.
//! * **Deterministic truncation.** If the output ever exceeds
//!   [`METRICS_BUF_LEN`] it is cut at the last complete line, so the
//!   scraper sees fewer samples rather than a half-written one. The
//!   tests assert the worst case fits, so this is a backstop.

use core::fmt::{self, Write};

use heapless::String;

use crate::persist_data::Snapshot;

/// Capacity of the rendered response. The worst case (every counter at
/// `u32::MAX`, uptime at `u64::MAX`, the longest label names) is about
/// 1.4 KiB; see `worst_case_fits_in_buffer`.
pub const METRICS_BUF_LEN: usize = 2048;

pub type MetricsBuf = String<METRICS_BUF_LEN>;

/// `Content-Type` for the Prometheus text exposition format. `0.0.4` is that
/// format's version (unchanged since Prometheus 0.4); the `version` parameter
/// is how a scraper tells it apart from OpenMetrics or protobuf.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Render `snap` in the Prometheus text exposition format.
pub fn render(snap: &Snapshot) -> MetricsBuf {
    let mut out = LineTruncatingWriter {
        buf: String::new(),
        overflowed: false,
    };
    // `write_all` only fails when the buffer overflows, which the
    // writer has already handled by truncating at a line boundary.
    let _ = write_all(snap, &mut out);
    out.buf
}

fn write_all(s: &Snapshot, w: &mut impl Write) -> fmt::Result {
    gauge(w, "pico_uptime_seconds", "Seconds since this boot.")?;
    writeln!(w, "pico_uptime_seconds {}", s.uptime_secs)?;

    gauge(
        w,
        "pico_free_heap_bytes",
        "Free heap at the last watchdog feed.",
    )?;
    writeln!(w, "pico_free_heap_bytes {}", s.last_free_heap)?;

    counter(
        w,
        "pico_heartbeat_total",
        "Per-task heartbeats; zeroed on every boot.",
    )?;
    for (task, value) in [
        ("api", s.heartbeats.api),
        ("display", s.heartbeats.display),
        ("watchdog", s.heartbeats.watchdog),
    ] {
        writeln!(w, "pico_heartbeat_total{{task=\"{task}\"}} {value}")?;
    }

    counter(
        w,
        "pico_reset_count",
        "Resets since power-on; survives warm resets.",
    )?;
    writeln!(w, "pico_reset_count {}", s.reset_count)?;

    counter(
        w,
        "pico_panic_count",
        "Panics since power-on; survives warm resets.",
    )?;
    writeln!(w, "pico_panic_count {}", s.panic_count)?;

    // A gauge, not a counter: consecutive failures, reset to zero on
    // any successful poll. `rate()` over it would be meaningless.
    gauge(
        w,
        "pico_api_fail_count",
        "Consecutive failed polls of the server.",
    )?;
    writeln!(w, "pico_api_fail_count {}", s.last_api_fail_count)?;

    gauge(w, "pico_network_up", "1 if the network link is up.")?;
    writeln!(w, "pico_network_up {}", u8::from(s.network_up))?;

    gauge(
        w,
        "pico_persist_magic_valid",
        "0 after a cold boot re-initialized the persist region.",
    )?;
    writeln!(w, "pico_persist_magic_valid {}", u8::from(s.magic_valid))?;

    gauge(w, "pico_app_state", "Current app state, as a label.")?;
    w.write_str("pico_app_state{name=\"")?;
    write_label_value(w, s.last_app_state.name)?;
    w.write_str("\"} 1\n")?;

    gauge(
        w,
        "pico_reset_reason",
        "Classified cause of this boot, as a label.",
    )?;
    w.write_str("pico_reset_reason{reason=\"")?;
    write_label_value(w, s.reset_reason.name())?;
    w.write_str("\"} 1\n")?;

    Ok(())
}

fn gauge(w: &mut impl Write, name: &str, help: &str) -> fmt::Result {
    writeln!(w, "# HELP {name} {help}\n# TYPE {name} gauge")
}

fn counter(w: &mut impl Write, name: &str, help: &str) -> fmt::Result {
    writeln!(w, "# HELP {name} {help}\n# TYPE {name} counter")
}

/// Escape a label value per the exposition format: backslash, double
/// quote and newline. Today's names never contain them, but the labels
/// come from strings rather than literals at this call site.
fn write_label_value(w: &mut impl Write, value: &str) -> fmt::Result {
    for c in value.chars() {
        match c {
            '\\' => w.write_str("\\\\")?,
            '"' => w.write_str("\\\"")?,
            '\n' => w.write_str("\\n")?,
            c => w.write_char(c)?,
        }
    }
    Ok(())
}

/// `fmt::Write` into a fixed buffer that, on the first write that does
/// not fit, rolls back to the end of the last complete line and refuses
/// everything after. Partial lines are what would make a scraper reject
/// the whole response.
struct LineTruncatingWriter {
    buf: MetricsBuf,
    overflowed: bool,
}

impl Write for LineTruncatingWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.overflowed {
            return Err(fmt::Error);
        }
        if self.buf.push_str(s).is_err() {
            self.overflowed = true;
            let keep = self.buf.rfind('\n').map_or(0, |i| i + 1);
            self.buf.truncate(keep);
            return Err(fmt::Error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::app_state_name;
    use crate::persist_data::{AppStateLabel, Heartbeats};
    use crate::reset::{init_stage_name, ResetReason};
    use heapless::Vec;

    fn snapshot() -> Snapshot {
        Snapshot {
            magic_valid: true,
            uptime_secs: 12345,
            reset_count: 7,
            panic_count: 3,
            last_displayed_panic_count: 2,
            message_is_new: true,
            reset_reason: ResetReason::WatchdogTimeout,
            last_app_state: AppStateLabel {
                id: 3,
                name: "Cooking",
            },
            last_uptime_secs: 600,
            heartbeats: Heartbeats {
                api: 100,
                display: 200,
                watchdog: 300,
            },
            last_free_heap: 28000,
            network_up: true,
            last_api_fail_count: 1,
            ring_head: 9,
            reset_history: Vec::new(),
            message: None,
        }
    }

    /// Every sample line, i.e. excluding `# HELP` / `# TYPE` comments.
    fn samples(out: &str) -> alloc::vec::Vec<&str> {
        out.lines().filter(|l| !l.starts_with('#')).collect()
    }

    #[test]
    fn renders_every_metric_from_the_doc() {
        let out = render(&snapshot());
        let expected = [
            "pico_uptime_seconds 12345",
            "pico_free_heap_bytes 28000",
            "pico_heartbeat_total{task=\"api\"} 100",
            "pico_heartbeat_total{task=\"display\"} 200",
            "pico_heartbeat_total{task=\"watchdog\"} 300",
            "pico_reset_count 7",
            "pico_panic_count 3",
            "pico_api_fail_count 1",
            "pico_network_up 1",
            "pico_persist_magic_valid 1",
            "pico_app_state{name=\"Cooking\"} 1",
            "pico_reset_reason{reason=\"WatchdogTimeout\"} 1",
        ];
        assert_eq!(samples(&out), expected);
    }

    #[test]
    fn every_sample_has_a_type_line_and_no_timestamp() {
        let out = render(&snapshot());
        for line in samples(&out) {
            let name = line.split(['{', ' ']).next().unwrap();
            let type_line_prefix = alloc::format!("# TYPE {name} ");
            assert!(
                out.lines().any(|l| l.starts_with(&type_line_prefix)),
                "no TYPE line for {name}"
            );
            // `name{labels} value` — a third whitespace-separated field
            // would be a timestamp, which the firmware must never emit.
            let after_labels = line.rsplit('}').next().unwrap();
            let fields = after_labels.split_whitespace().count();
            let expected = if line.contains('}') { 1 } else { 2 };
            assert_eq!(fields, expected, "unexpected fields in `{line}`");
        }
    }

    #[test]
    fn booleans_render_as_zero() {
        let mut snap = snapshot();
        snap.network_up = false;
        snap.magic_valid = false;
        let out = render(&snap);
        assert!(out.contains("\npico_network_up 0\n"));
        assert!(out.contains("\npico_persist_magic_valid 0\n"));
    }

    #[test]
    fn label_values_are_escaped() {
        let mut out: String<64> = String::new();
        write_label_value(&mut out, "a\"b\\c\nd").unwrap();
        assert_eq!(out.as_str(), "a\\\"b\\\\c\\nd");
    }

    /// The size bound the doc asks for, computed once: every numeric
    /// field at its maximum, crossed with every label value the closed
    /// enums can produce.
    #[test]
    fn worst_case_fits_in_buffer() {
        let reasons = [
            ResetReason::Unknown,
            ResetReason::ColdBoot,
            ResetReason::Panic,
            ResetReason::WatchdogTimeout,
            ResetReason::WatchdogForced,
            ResetReason::OtherSoftReset,
            ResetReason::InitTimeout,
        ];
        // Every discriminant `AppStateLabel::from_discriminant` can
        // name, plus one it can't (-> "Unknown").
        let states = (0..=255u32)
            .filter(|d| app_state_name(*d).is_some() || init_stage_name(*d).is_some())
            .chain([u32::MAX]);

        let mut worst = 0;
        for d in states {
            for reason in reasons {
                let mut snap = snapshot();
                snap.uptime_secs = u64::MAX;
                snap.reset_count = u32::MAX;
                snap.panic_count = u32::MAX;
                snap.last_free_heap = u32::MAX;
                snap.last_api_fail_count = u32::MAX;
                snap.heartbeats = Heartbeats {
                    api: u32::MAX,
                    display: u32::MAX,
                    watchdog: u32::MAX,
                };
                snap.reset_reason = reason;
                snap.last_app_state = AppStateLabel::from_discriminant(d);
                let out = render(&snap);
                assert!(
                    out.ends_with("} 1\n"),
                    "truncated at {} bytes: {out}",
                    out.len()
                );
                worst = worst.max(out.len());
            }
        }
        // Keep real headroom so adding a metric doesn't silently start
        // truncating the last one.
        assert!(
            worst * 4 <= METRICS_BUF_LEN * 3,
            "worst case {worst} B is within 25% of METRICS_BUF_LEN"
        );
    }

    #[test]
    fn overflow_truncates_at_a_line_boundary() {
        let mut w = LineTruncatingWriter {
            buf: String::new(),
            overflowed: false,
        };
        let line = "0123456789abcdef0123456789abcde\n"; // 32 bytes
        let mut written = 0;
        while w.write_str(line).is_ok() {
            written += 1;
        }
        // A partial line followed by a rejected write must not survive.
        let _ = w.write_str("tail");
        assert_eq!(w.buf.len(), written * line.len());
        assert!(w.buf.ends_with('\n'));

        let mut w = LineTruncatingWriter {
            buf: String::new(),
            overflowed: false,
        };
        let _ = w.write_str("complete\npartial");
        let big = [b'x'; METRICS_BUF_LEN];
        let _ = w.write_str(core::str::from_utf8(&big).unwrap());
        assert_eq!(w.buf.as_str(), "complete\n");
    }
}
