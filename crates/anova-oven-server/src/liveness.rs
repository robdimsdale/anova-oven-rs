//! Upstream liveness tracking for the `/health` endpoint.
//!
//! The failure that motivated this: the WebSocket to Anova's cloud can go
//! quiet (half-open socket, dead token, cloud outage) while the server keeps
//! answering `GET /status` with the last cached reading. Every downstream
//! consumer sees HTTP 200 and stale data with nothing to react to. This tracker
//! records when the last real oven-state frame arrived and whether the socket
//! is currently connected, so an external monitor can alert on staleness that
//! the in-process watchdog can't (e.g. the process is wedged, not just the
//! socket).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Shared, lock-free liveness state. Written by the WebSocket processor, read
/// by the `/health` HTTP handler.
#[derive(Debug)]
pub struct Liveness {
    /// Unix-epoch milliseconds of the last `EVENT_APO_STATE` received from
    /// Anova. `0` means none has arrived since startup.
    last_state_ms: AtomicU64,
    /// Whether the upstream WebSocket is currently connected.
    connected: AtomicBool,
    /// Whether the oven itself is reaching Anova's cloud. The socket to the
    /// cloud can be up while the oven is not: the cloud then answers with its
    /// last cached state and drops commands. Only the cloud processor clears
    /// this; it starts `true` so local mode never sees it.
    oven_online: AtomicBool,
    /// Unix-epoch milliseconds at which the link last transitioned from
    /// reachable (socket connected and oven online) to unreachable, or at
    /// which the server started. `0` means connected. Used
    /// to report *how long* the link has been down so a display client can
    /// ignore the routine ~5s max-connection-age reconnects.
    disconnected_since_ms: AtomicU64,
    /// The configured read-timeout window, echoed into `/health` so a monitor
    /// knows the bound past which the in-process watchdog would have already
    /// forced a reconnect.
    read_timeout_secs: u64,
}

impl Liveness {
    pub fn new(read_timeout_secs: u64) -> Self {
        Self {
            last_state_ms: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            oven_online: AtomicBool::new(true),
            // Down from the start: an oven that never connects (local mode,
            // oven unplugged) must count as an outage too.
            disconnected_since_ms: AtomicU64::new(now_ms()),
            read_timeout_secs,
        }
    }

    /// Record that an oven-state frame just arrived.
    pub fn record_state(&self) {
        self.last_state_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Record the current connection status of the upstream socket. Only ever
    /// called from the single WS task, so the read-modify-write below needs no
    /// external synchronization.
    pub fn set_connected(&self, connected: bool) {
        let was = self.connected();
        self.connected.store(connected, Ordering::Relaxed);
        self.track_transition(was);
    }

    /// Record whether the oven is reaching Anova's cloud, as far as the frames
    /// the cloud relays tell. Same single-writer rule as [`Self::set_connected`].
    pub fn set_oven_online(&self, online: bool) {
        let was = self.connected();
        self.oven_online.store(online, Ordering::Relaxed);
        self.track_transition(was);
    }

    fn track_transition(&self, was: bool) {
        if self.connected() {
            // Back up: clear any in-flight disconnect timer.
            self.disconnected_since_ms.store(0, Ordering::Relaxed);
        } else if was {
            // Just transitioned connected -> disconnected: start the timer.
            // Repeated set_connected(false) during a reconnect storm keeps the
            // original timestamp (the `else if was` guard), so the duration
            // reflects the whole outage, not the last 5s retry.
            self.disconnected_since_ms
                .store(now_ms(), Ordering::Relaxed);
        }
    }

    /// Seconds the link has been continuously disconnected, counting from
    /// startup before the first connect, or `0` when connected.
    pub fn disconnected_secs(&self) -> u64 {
        let since = self.disconnected_since_ms.load(Ordering::Relaxed);
        if since == 0 {
            0
        } else {
            now_ms().saturating_sub(since) / 1000
        }
    }

    /// Unix-epoch milliseconds of the last oven-state frame, or `None` if none
    /// has arrived since startup. `/metrics` publishes this as an absolute
    /// timestamp rather than an age (see `telemetry::record_liveness`).
    pub fn last_state_unix_ms(&self) -> Option<u64> {
        match self.last_state_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// Whether the oven is reachable: the upstream WebSocket is connected and,
    /// in cloud mode, the oven is online behind it.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed) && self.oven_online.load(Ordering::Relaxed)
    }

    /// Point-in-time view for serialization.
    pub fn snapshot(&self) -> LivenessSnapshot {
        let seconds_since_last_state = self
            .last_state_unix_ms()
            .map(|last| now_ms().saturating_sub(last) / 1000);
        LivenessSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            oven_online: self.oven_online.load(Ordering::Relaxed),
            seconds_since_last_state,
            disconnected_secs: self.disconnected_secs(),
            read_timeout_secs: self.read_timeout_secs,
        }
    }
}

/// JSON body served by `GET /health`.
#[derive(Debug, Serialize)]
pub struct LivenessSnapshot {
    /// Is the upstream WebSocket currently connected?
    pub connected: bool,
    /// Is the oven reaching Anova's cloud? `false` while the socket is up but
    /// the cloud is only replaying its cache.
    pub oven_online: bool,
    /// Seconds since the last fresh oven-state frame, or `null` if none yet. A value
    /// climbing toward `read_timeout_secs` means upstream has gone quiet.
    pub seconds_since_last_state: Option<u64>,
    /// Seconds the WebSocket has been continuously disconnected (`0` when
    /// connected). Mirrors what `/status` surfaces to display clients.
    pub disconnected_secs: u64,
    /// The in-process read-timeout bound, for context.
    pub read_timeout_secs: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_liveness_reports_never_connected_and_no_state() {
        let l = Liveness::new(1200);
        let snap = l.snapshot();
        assert!(!snap.connected);
        assert_eq!(snap.seconds_since_last_state, None);
        assert_eq!(snap.read_timeout_secs, 1200);
    }

    #[test]
    fn recording_state_and_connection_is_reflected() {
        let l = Liveness::new(600);
        l.set_connected(true);
        l.record_state();
        let snap = l.snapshot();
        assert!(snap.connected);
        // Just recorded, so elapsed whole seconds should be 0.
        assert_eq!(snap.seconds_since_last_state, Some(0));
        assert_eq!(snap.read_timeout_secs, 600);
    }

    #[test]
    fn disconnected_secs_is_zero_while_connected() {
        let l = Liveness::new(60);
        // Never connected yet: counting from startup, still within its first
        // second.
        assert_eq!(l.disconnected_secs(), 0);
        assert_ne!(l.disconnected_since_ms.load(Ordering::Relaxed), 0);
        l.set_connected(true);
        assert_eq!(l.disconnected_secs(), 0);
    }

    #[test]
    fn disconnect_starts_timer_and_reconnect_clears_it() {
        let l = Liveness::new(60);
        l.set_connected(true);
        l.set_connected(false);
        assert!(!l.connected());
        // Just disconnected, same second -> 0, but the timer is now armed.
        assert_eq!(l.disconnected_secs(), 0);
        // A reconnect storm (repeated set_connected(false)) must not re-arm the
        // timer; it stays measuring from the original drop. We can't advance the
        // wall clock here, so assert it doesn't blow up or flip to connected.
        l.set_connected(false);
        l.set_connected(false);
        assert!(!l.connected());
        // Reconnecting clears the duration.
        l.set_connected(true);
        assert!(l.connected());
        assert_eq!(l.disconnected_secs(), 0);
    }

    #[test]
    fn an_offline_oven_counts_as_disconnected_until_it_returns() {
        let l = Liveness::new(60);
        l.set_connected(true);
        l.set_oven_online(false);
        assert!(!l.connected());
        assert!(!l.snapshot().oven_online);
        // A reconnect of the socket alone doesn't bring the oven back.
        l.set_connected(false);
        l.set_connected(true);
        assert!(!l.connected());
        l.set_oven_online(true);
        assert!(l.connected());
        assert_eq!(l.disconnected_secs(), 0);
    }

    #[test]
    fn disconnect_clears_connected_but_keeps_last_state() {
        let l = Liveness::new(60);
        l.record_state();
        l.set_connected(true);
        l.set_connected(false);
        let snap = l.snapshot();
        assert!(!snap.connected);
        // Last-state timestamp survives a disconnect so `/health` can show how
        // long data has been stale even while reconnecting.
        assert_eq!(snap.seconds_since_last_state, Some(0));
    }
}
