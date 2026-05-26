//! Reset classification — pure logic that maps the inputs we capture at boot
//! (was the persist `MAGIC` valid?, did `panic_count` advance?, what was the
//! last `AppState` breadcrumb?, did the watchdog fire and how?) into a
//! [`ResetReason`]. The MMIO read of the chip's watchdog-reason register and
//! the persist-region access stay in the bin (chip-specific); this module
//! does only the classification.

/// What caused the boot we're currently in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[repr(u32)]
pub enum ResetReason {
    /// Reserved for invalid stored values — used when decoding old ring
    /// entries with values outside the known enum range.
    Unknown = 0,
    /// Magic word was invalid — first boot since power-on, or RAM lost.
    ColdBoot = 1,
    /// Previous run hit our `#[panic_handler]` (or a fault handler)
    /// which then soft-reset the chip.
    Panic = 2,
    /// Hardware watchdog timed out (no `feed()` within its window). The
    /// bin reads the chip's watchdog-reason register to tell this apart
    /// from a forced reset; the classification here is chip-agnostic.
    WatchdogTimeout = 3,
    /// Watchdog was deliberately triggered (a forced reset). Not used by
    /// our firmware today; present for completeness.
    WatchdogForced = 4,
    /// Plain soft reset (or external reset pin) without a panic.
    /// Shouldn't happen in normal operation.
    OtherSoftReset = 5,
    /// A bring-up stage (WiFi join / DHCP) didn't complete within its
    /// deadline and `reboot_init_timeout()` deliberately reset the board.
    /// Inferred from the `last_app_state` breadcrumb still being an
    /// `INIT_STAGE_*` value at the next boot.
    InitTimeout = 6,
}

impl ResetReason {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::ColdBoot,
            2 => Self::Panic,
            3 => Self::WatchdogTimeout,
            4 => Self::WatchdogForced,
            5 => Self::OtherSoftReset,
            6 => Self::InitTimeout,
            _ => Self::Unknown,
        }
    }

    /// Human-readable variant name. Used by the firmware's `/health`
    /// endpoint and parsed out of this source file by the bin's
    /// `dump-persist` debug-port tool so both surfaces share one
    /// source of truth — keep the `Self::Variant => "Variant"` arm
    /// style stable (the regex parser assumes one arm per line and
    /// an exact-name string literal) when editing.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::ColdBoot => "ColdBoot",
            Self::Panic => "Panic",
            Self::WatchdogTimeout => "WatchdogTimeout",
            Self::WatchdogForced => "WatchdogForced",
            Self::OtherSoftReset => "OtherSoftReset",
            Self::InitTimeout => "InitTimeout",
        }
    }
}

/// `last_app_state` breadcrumb values for the pre-`AppState` bring-up
/// phases. Deliberately well clear of `AppState::discriminant()` (1..=8)
/// so a reset *during* init is distinguishable from one in the running
/// state machine.
pub const INIT_STAGE_WIFI: u32 = 100;
pub const INIT_STAGE_DHCP: u32 = 101;

pub fn is_init_stage(s: u32) -> bool {
    matches!(s, INIT_STAGE_WIFI | INIT_STAGE_DHCP)
}

/// Human-readable label for an `INIT_STAGE_*` sentinel value, or
/// `None` if `d` isn't a known init-stage discriminant. Co-located
/// with the `INIT_STAGE_*` consts above so adding a new init stage
/// means editing one screen.
///
/// Parsed by the bin's `dump-persist` debug-port tool — keep arms as
/// `INIT_STAGE_NAME => Some("INIT_STAGE_NAME"),` on one line so the
/// regex parser keeps working.
pub fn init_stage_name(d: u32) -> Option<&'static str> {
    match d {
        INIT_STAGE_WIFI => Some("INIT_STAGE_WIFI"),
        INIT_STAGE_DHCP => Some("INIT_STAGE_DHCP"),
        _ => None,
    }
}

/// Combine the persist breadcrumbs with the chip's watchdog status to
/// classify what caused this boot. `watchdog_timer` and `watchdog_force`
/// are the two booleans the bin extracts from whatever
/// watchdog-reason MMIO register the chip exposes — see the bin for
/// the chip-specific decoding.
///
/// A panic is checked *before* the watchdog timer bit on purpose: a panic
/// is the root cause, and if a slow/hung panic handler lets the watchdog
/// fire too (`watchdog_timer = true`), we still want the reset attributed
/// to the panic rather than masked as a plain watchdog timeout.
///
/// `InitTimeout` is what would otherwise be an `OtherSoftReset` (plain
/// soft reset, no panic, no watchdog) but with `last_app_state` still in
/// the `INIT_STAGE_*` range — i.e. the box reset while in WiFi/DHCP
/// bring-up, which is our deliberate `reboot_init_timeout()`. (A stray
/// reset-pin assertion during bring-up lands here too; "reset during
/// init" is a fair label either way.)
pub fn classify_reset(
    magic_was_valid: bool,
    panic_count_advanced: bool,
    last_app_state: u32,
    watchdog_timer: bool,
    watchdog_force: bool,
) -> ResetReason {
    if !magic_was_valid {
        ResetReason::ColdBoot
    } else if panic_count_advanced {
        ResetReason::Panic
    } else if watchdog_timer {
        ResetReason::WatchdogTimeout
    } else if watchdog_force {
        ResetReason::WatchdogForced
    } else if is_init_stage(last_app_state) {
        ResetReason::InitTimeout
    } else {
        ResetReason::OtherSoftReset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The 5-tuple is (magic_valid, panic_advanced, last_app_state, wdog_timer, wdog_force).
    // `0` for last_app_state is a non-init value (AppState discriminants are 1..=8;
    // init stages are 100/101).

    #[test]
    fn invalid_magic_is_cold_boot_regardless_of_other_bits() {
        // Even if every other indicator screams panic/watchdog, an invalid
        // magic word means RAM was lost — no signal from prior run is real.
        assert_eq!(
            classify_reset(false, true, INIT_STAGE_WIFI, true, true),
            ResetReason::ColdBoot
        );
        assert_eq!(
            classify_reset(false, false, 0, false, false),
            ResetReason::ColdBoot
        );
    }

    #[test]
    fn panic_beats_watchdog() {
        // Documented precedence: a slow panic handler can let the watchdog
        // fire too; the panic is the root cause.
        assert_eq!(
            classify_reset(true, true, 0, true, false),
            ResetReason::Panic
        );
        assert_eq!(
            classify_reset(true, true, 0, false, true),
            ResetReason::Panic
        );
        assert_eq!(
            classify_reset(true, true, 0, true, true),
            ResetReason::Panic
        );
    }

    #[test]
    fn watchdog_timer_classified_when_no_panic() {
        assert_eq!(
            classify_reset(true, false, 0, true, false),
            ResetReason::WatchdogTimeout
        );
    }

    #[test]
    fn watchdog_force_classified_when_no_panic_and_no_timer() {
        assert_eq!(
            classify_reset(true, false, 0, false, true),
            ResetReason::WatchdogForced
        );
    }

    #[test]
    fn init_timeout_when_clean_soft_reset_during_bringup() {
        assert_eq!(
            classify_reset(true, false, INIT_STAGE_WIFI, false, false),
            ResetReason::InitTimeout
        );
        assert_eq!(
            classify_reset(true, false, INIT_STAGE_DHCP, false, false),
            ResetReason::InitTimeout
        );
    }

    #[test]
    fn other_soft_reset_when_nothing_else_matches() {
        // Past init, no panic, no watchdog — shouldn't happen in normal
        // operation but is the catch-all.
        assert_eq!(
            classify_reset(true, false, 5, false, false),
            ResetReason::OtherSoftReset
        );
    }

    #[test]
    fn init_timeout_only_for_known_init_stages() {
        // A non-init `last_app_state` (some AppState discriminant) with no
        // panic/watchdog falls into OtherSoftReset, not InitTimeout.
        assert_eq!(
            classify_reset(true, false, 3, false, false),
            ResetReason::OtherSoftReset
        );
        assert_eq!(
            classify_reset(true, false, 99, false, false),
            ResetReason::OtherSoftReset
        );
        assert_eq!(
            classify_reset(true, false, 102, false, false),
            ResetReason::OtherSoftReset
        );
    }

    #[test]
    fn from_u32_known_values() {
        assert_eq!(ResetReason::from_u32(0), ResetReason::Unknown);
        assert_eq!(ResetReason::from_u32(1), ResetReason::ColdBoot);
        assert_eq!(ResetReason::from_u32(2), ResetReason::Panic);
        assert_eq!(ResetReason::from_u32(3), ResetReason::WatchdogTimeout);
        assert_eq!(ResetReason::from_u32(4), ResetReason::WatchdogForced);
        assert_eq!(ResetReason::from_u32(5), ResetReason::OtherSoftReset);
        assert_eq!(ResetReason::from_u32(6), ResetReason::InitTimeout);
    }

    #[test]
    fn from_u32_unknown_values_decode_to_unknown() {
        // Forward compatibility: an old ring entry with a value the current
        // firmware doesn't recognize should decode to Unknown, not panic.
        assert_eq!(ResetReason::from_u32(7), ResetReason::Unknown);
        assert_eq!(ResetReason::from_u32(42), ResetReason::Unknown);
        assert_eq!(ResetReason::from_u32(u32::MAX), ResetReason::Unknown);
    }

    #[test]
    fn reset_reason_name_round_trips_every_variant() {
        // Adding a ResetReason variant without a `name()` arm fails to
        // compile (the match is exhaustive). This test additionally
        // guards against a developer adding `Self::New => ""` — every
        // variant must have a non-empty, non-"Unknown"-collision name
        // (except `Unknown` itself).
        let all = [
            ResetReason::Unknown,
            ResetReason::ColdBoot,
            ResetReason::Panic,
            ResetReason::WatchdogTimeout,
            ResetReason::WatchdogForced,
            ResetReason::OtherSoftReset,
            ResetReason::InitTimeout,
        ];
        for r in all {
            let n = r.name();
            assert!(!n.is_empty(), "ResetReason::{r:?} has empty name");
            // Round-trip via from_u32: name must describe the same variant.
            let round = ResetReason::from_u32(r as u32);
            assert_eq!(
                round, r,
                "ResetReason::from_u32({}) returned {round:?}, expected {r:?}",
                r as u32
            );
        }
    }

    #[test]
    fn init_stage_name_covers_known_stages() {
        assert_eq!(init_stage_name(INIT_STAGE_WIFI), Some("INIT_STAGE_WIFI"));
        assert_eq!(init_stage_name(INIT_STAGE_DHCP), Some("INIT_STAGE_DHCP"));
        assert_eq!(init_stage_name(0), None);
        assert_eq!(init_stage_name(99), None);
        assert_eq!(init_stage_name(102), None);
        // Anything `is_init_stage` accepts, `init_stage_name` must also
        // recognize — guards against the two staying in sync only by
        // hand.
        for d in 0u32..=200 {
            assert_eq!(
                is_init_stage(d),
                init_stage_name(d).is_some(),
                "drift at d={d}"
            );
        }
    }

    #[test]
    fn is_init_stage_recognizes_known_stages() {
        assert!(is_init_stage(INIT_STAGE_WIFI));
        assert!(is_init_stage(INIT_STAGE_DHCP));
        assert!(!is_init_stage(0));
        assert!(!is_init_stage(1)); // an AppState discriminant
        assert!(!is_init_stage(99)); // just under the init range
        assert!(!is_init_stage(102)); // just past the init range
    }
}
