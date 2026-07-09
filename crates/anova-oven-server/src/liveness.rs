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
            read_timeout_secs,
        }
    }

    /// Record that an oven-state frame just arrived.
    pub fn record_state(&self) {
        self.last_state_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Record the current connection status of the upstream socket.
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    /// Point-in-time view for serialization.
    pub fn snapshot(&self) -> LivenessSnapshot {
        let last = self.last_state_ms.load(Ordering::Relaxed);
        let seconds_since_last_state = if last == 0 {
            None
        } else {
            Some(now_ms().saturating_sub(last) / 1000)
        };
        LivenessSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            seconds_since_last_state,
            read_timeout_secs: self.read_timeout_secs,
        }
    }
}

/// JSON body served by `GET /health`.
#[derive(Debug, Serialize)]
pub struct LivenessSnapshot {
    /// Is the upstream WebSocket currently connected?
    pub connected: bool,
    /// Seconds since the last oven-state frame, or `null` if none yet. A value
    /// climbing toward `read_timeout_secs` means upstream has gone quiet.
    pub seconds_since_last_state: Option<u64>,
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
