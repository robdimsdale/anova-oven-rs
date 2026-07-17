//! Data shape of the firmware's persisted breadcrumb region.
//!
//! The actual MMIO read/write and `#[panic_handler]` live in the bin's
//! persist module — that part is chip-specific and depends on the
//! `.uninit.PERSIST` static. This module defines only the data layout
//! the bin's reader returns and the `/health` endpoint serves.
//!
//! Why it lives here rather than in the bin:
//!
//! * Host-testable. The round-trip serialize test below catches
//!   accidental `#[serde(skip)]` or wrong-rename bugs at `cargo test`
//!   time — the bin's standalone embedded workspace can't run host
//!   tests.
//! * Zero-drift with `/health`. The bin's `/health` handler serves
//!   `Json(read_live())` directly. Adding a field to [`Snapshot`]
//!   automatically appears in the JSON because `Serialize` is derived
//!   — there is no parallel response struct that can fall behind.
//! * Shared sizing constants. [`MSG_BUF_SIZE`] and [`RING_SIZE`] are
//!   the canonical contract values used by both the bin's
//!   `PersistRegion` layout and any consumer iterating the ring or
//!   the message buffer. Changing them requires bumping `MAGIC` in
//!   the bin (see persist.rs in the bin) so old in-RAM data fails the
//!   magic check after a firmware update.

use heapless::{String, Vec};
#[cfg(feature = "serde")]
use serde::Serialize;

use crate::fsm::app_state_name;
use crate::reset::{init_stage_name, ResetReason};

/// Canonical sizing constants for the persist region. Bin's
/// `PersistRegion` layout must match — bump `MAGIC` in the bin if any
/// of these changes.
pub const MSG_BUF_SIZE: usize = 512;
pub const RING_SIZE: usize = 8;
/// Bytes reserved for the NUL-padded build-version string. 48 holds
/// `"<pkg>-<8-hex-sha>-dirty\0"` (≈22 B) with headroom.
pub const VERSION_BUF_SIZE: usize = 48;

/// One entry in the boot-history ring. Mirrors the on-MMIO layout
/// (6× u32 in the bin's `RingEntry`) but exposes booleans / typed
/// enums to consumers.
#[derive(Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ResetHistoryEntry {
    pub reset_reason: ResetReason,
    pub uptime_secs: u32,
    pub api_heartbeat: u32,
    pub free_heap: u32,
    pub network_up: bool,
    pub api_fail_count: u32,
}

/// Decoded `last_app_state` breadcrumb: the raw u32 alongside its
/// human-readable label. Computed at read time so the JSON consumer
/// and the bin's `info!` logging see the same name, and so an unknown
/// value (e.g. an added `AppState` variant + missing
/// [`app_state_name`] arm) shows as `"Unknown"` rather than silently
/// dropping out.
#[derive(Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct AppStateLabel {
    pub id: u32,
    pub name: &'static str,
}

impl AppStateLabel {
    /// Look up the label for a `last_app_state` value, falling back
    /// in order: `AppState::discriminant()` → `INIT_STAGE_*` →
    /// `"Unknown"`.
    pub fn from_discriminant(d: u32) -> Self {
        Self {
            id: d,
            name: app_state_name(d)
                .or_else(|| init_stage_name(d))
                .unwrap_or("Unknown"),
        }
    }
}

/// Heartbeat counters maintained by each long-running task. Grouped
/// so they show up together in `/health` JSON and stay easy to scan in
/// logs (vs. nine separate top-level u32s).
#[derive(Copy, Clone, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Heartbeats {
    pub api: u32,
    pub display: u32,
    pub watchdog: u32,
}

/// Decoded view of the entire persist region as of one specific
/// instant. Used both at boot (`init_at_boot`, which captures the
/// snapshot *after* the boot-time mutations have run) and at runtime
/// (`read_live`, which the `/health` endpoint serializes directly).
///
/// **Drift contract**: this struct's `Serialize` impl IS the
/// `/health` JSON schema. Adding a `pub` field here automatically
/// surfaces it in `/health`; renaming or `#[serde(skip)]`ing one is
/// what the round-trip test below guards against.
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Snapshot {
    /// `false` indicates a cold boot (RAM lost) — the region was just
    /// re-initialized and every numeric field below is 0/synthetic.
    pub magic_valid: bool,
    /// Current run's uptime — taken from `Instant::now()` at read
    /// time, not from the `last_uptime_secs` breadcrumb (which only
    /// updates each watchdog feed).
    pub uptime_secs: u64,
    pub reset_count: u32,
    pub panic_count: u32,
    pub last_displayed_panic_count: u32,
    /// True when `panic_count` advanced past `last_displayed_panic_count`
    /// — i.e. a panic happened since the last LCD recovery view.
    pub message_is_new: bool,
    /// Boot-classified reset reason (set once by `init_at_boot`).
    pub reset_reason: ResetReason,
    pub last_app_state: AppStateLabel,
    /// Previous run's last `Instant::now().as_secs()` at the watchdog
    /// feed — i.e. how long the run that just ended had been alive.
    pub last_uptime_secs: u32,
    pub heartbeats: Heartbeats,
    pub last_free_heap: u32,
    pub network_up: bool,
    pub last_api_fail_count: u32,
    pub ring_head: u32,
    /// The last RING_SIZE resets, newest first. Includes the reset
    /// that produced *this* boot at index 0 (unless it was a cold
    /// boot).
    pub reset_history: Vec<ResetHistoryEntry, RING_SIZE>,
    /// Last persisted panic message, if any. Decoded as a UTF-8
    /// prefix of the on-MMIO `msg_buf`; truncated silently if the
    /// stored bytes don't form valid UTF-8 past some prefix.
    #[cfg_attr(feature = "serde", serde(rename = "panic_message"))]
    pub message: Option<String<MSG_BUF_SIZE>>,
    /// Build version of the *currently running* image:
    /// `"<CARGO_PKG_VERSION>-<short-git-sha>[-dirty]"`. Recorded into
    /// the persist MMIO version slot at every boot, so the value
    /// reported here also survives across resets and is readable via
    /// dump-persist over SWD even if `/health` is unreachable.
    pub version: String<VERSION_BUF_SIZE>,
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;
    use alloc::string::ToString;

    /// Build a sentinel Snapshot with distinct values in every field
    /// so the round-trip test can prove each one round-trips through
    /// JSON.
    fn sentinel_snapshot() -> Snapshot {
        let mut history: Vec<ResetHistoryEntry, RING_SIZE> = Vec::new();
        let _ = history.push(ResetHistoryEntry {
            reset_reason: ResetReason::Panic,
            uptime_secs: 111,
            api_heartbeat: 222,
            free_heap: 333,
            network_up: true,
            api_fail_count: 4,
        });
        let mut msg: String<MSG_BUF_SIZE> = String::new();
        let _ = msg.push_str("hello panic");
        let mut version: String<VERSION_BUF_SIZE> = String::new();
        let _ = version.push_str("0.1.0-deadbeef-dirty");
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
            reset_history: history,
            message: Some(msg),
            version,
        }
    }

    /// Locks the JSON shape against silent drift. We parse the
    /// serialized output and assert every key we *expect* to be
    /// present is there with the right type/value. If a future change
    /// adds `#[serde(skip)]` to a field, renames one without updating
    /// docs, or hand-rolls a `Serialize` impl that drops a field,
    /// this test fails — i.e. you can't ship a persist change that
    /// silently disappears from `/health`.
    #[test]
    fn snapshot_json_contains_every_expected_field() {
        let snap = sentinel_snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse back");
        let obj = v.as_object().expect("top level is object");

        // Each (key, expected sentinel) — keep in sync with
        // `sentinel_snapshot()`. Note: `message` serializes as
        // `panic_message` thanks to the `#[serde(rename)]` attribute.
        let expectations: &[(&str, serde_json::Value)] = &[
            ("magic_valid", true.into()),
            ("uptime_secs", 12345.into()),
            ("reset_count", 7.into()),
            ("panic_count", 3.into()),
            ("last_displayed_panic_count", 2.into()),
            ("message_is_new", true.into()),
            // unit-variant enum -> string
            ("reset_reason", "WatchdogTimeout".into()),
            ("last_uptime_secs", 600.into()),
            ("last_free_heap", 28000.into()),
            ("network_up", true.into()),
            ("last_api_fail_count", 1.into()),
            ("ring_head", 9.into()),
            ("panic_message", "hello panic".into()),
            ("version", "0.1.0-deadbeef-dirty".into()),
        ];
        for (key, expected) in expectations {
            let got = obj.get(*key).unwrap_or_else(|| {
                panic!(
                    "/health JSON missing field `{key}` — \
                     drift between Snapshot and the response shape. \
                     Serialized output was: {json}"
                )
            });
            assert_eq!(
                got, expected,
                "/health JSON field `{key}` had unexpected value"
            );
        }

        // Nested fields:
        let app = obj
            .get("last_app_state")
            .and_then(|v| v.as_object())
            .expect("last_app_state object");
        assert_eq!(app.get("id"), Some(&serde_json::Value::from(3)));
        assert_eq!(app.get("name"), Some(&serde_json::Value::from("Cooking")));

        let hb = obj
            .get("heartbeats")
            .and_then(|v| v.as_object())
            .expect("heartbeats object");
        assert_eq!(hb.get("api"), Some(&serde_json::Value::from(100)));
        assert_eq!(hb.get("display"), Some(&serde_json::Value::from(200)));
        assert_eq!(hb.get("watchdog"), Some(&serde_json::Value::from(300)));

        let hist = obj
            .get("reset_history")
            .and_then(|v| v.as_array())
            .expect("reset_history array");
        assert_eq!(hist.len(), 1);
        let entry = hist[0].as_object().expect("ring entry");
        assert_eq!(
            entry.get("reset_reason"),
            Some(&serde_json::Value::from("Panic"))
        );
        assert_eq!(
            entry.get("uptime_secs"),
            Some(&serde_json::Value::from(111))
        );
        assert_eq!(
            entry.get("api_heartbeat"),
            Some(&serde_json::Value::from(222))
        );
        assert_eq!(entry.get("free_heap"), Some(&serde_json::Value::from(333)));
        assert_eq!(
            entry.get("network_up"),
            Some(&serde_json::Value::from(true))
        );
        assert_eq!(
            entry.get("api_fail_count"),
            Some(&serde_json::Value::from(4))
        );

        // Belt-and-suspenders: count the number of top-level keys
        // matches the number of `pub` fields on `Snapshot`. If a new
        // field is added in Rust, this count goes up and the
        // expectation list above goes stale — the test fails until
        // the new key is added to the expectations or counted in
        // `EXTRA_NESTED_KEYS`. Forces an active acknowledgment when
        // the schema grows.
        //
        // 14 flat + last_app_state + heartbeats + reset_history = 17.
        assert_eq!(
            obj.len(),
            17,
            "Snapshot grew a new field — add it to the expectation \
             list above and bump this count. Current keys: {:?}",
            obj.keys().collect::<alloc::vec::Vec<_>>()
        );

        // Touch `ToString` so the `use` is materially exercised even
        // if a future edit removes a `to_string()` call above.
        let _ = json.to_string();
    }
}
