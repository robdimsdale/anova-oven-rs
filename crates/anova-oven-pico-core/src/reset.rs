//! Reset classification — pure logic that maps the inputs we capture at boot
//! (was the persist `MAGIC` valid?, did `panic_count` advance?, what was the
//! last `AppState` breadcrumb?, what does the WATCHDOG.REASON register say?)
//! into a [`ResetReason`]. The MMIO read of WATCHDOG.REASON and the persist
//! region access stay in the bin; this module does only the classification.

/// What caused the boot we're currently in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u32)]
pub enum ResetReason {
    /// Reserved for invalid stored values — used when decoding old ring
    /// entries with values outside the known enum range.
    Unknown = 0,
    /// Magic word was invalid — first boot since power-on, or RAM lost.
    ColdBoot = 1,
    /// Previous run hit our `#[panic_handler]` (or HardFault) which then
    /// called `SCB::sys_reset()`.
    Panic = 2,
    /// RP2040 watchdog timed out (no `feed()` within the timeout window).
    WatchdogTimeout = 3,
    /// `Watchdog::trigger_reset()` was called explicitly. Not used by our
    /// firmware today; present for completeness.
    WatchdogForced = 4,
    /// `SCB::sys_reset()` (or external NRST) without a panic. Shouldn't
    /// happen in normal operation.
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

/// Combine the persist breadcrumbs with WATCHDOG.REASON to classify what
/// caused this boot.
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
/// NRST during bring-up lands here too; "reset during init" is a fair
/// label either way.)
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
        assert_eq!(classify_reset(false, true, INIT_STAGE_WIFI, true, true), ResetReason::ColdBoot);
        assert_eq!(classify_reset(false, false, 0, false, false), ResetReason::ColdBoot);
    }

    #[test]
    fn panic_beats_watchdog() {
        // Documented precedence: a slow panic handler can let the watchdog
        // fire too; the panic is the root cause.
        assert_eq!(classify_reset(true, true, 0, true, false), ResetReason::Panic);
        assert_eq!(classify_reset(true, true, 0, false, true), ResetReason::Panic);
        assert_eq!(classify_reset(true, true, 0, true, true), ResetReason::Panic);
    }

    #[test]
    fn watchdog_timer_classified_when_no_panic() {
        assert_eq!(classify_reset(true, false, 0, true, false), ResetReason::WatchdogTimeout);
    }

    #[test]
    fn watchdog_force_classified_when_no_panic_and_no_timer() {
        assert_eq!(classify_reset(true, false, 0, false, true), ResetReason::WatchdogForced);
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
        assert_eq!(classify_reset(true, false, 5, false, false), ResetReason::OtherSoftReset);
    }

    #[test]
    fn init_timeout_only_for_known_init_stages() {
        // A non-init `last_app_state` (some AppState discriminant) with no
        // panic/watchdog falls into OtherSoftReset, not InitTimeout.
        assert_eq!(classify_reset(true, false, 3, false, false), ResetReason::OtherSoftReset);
        assert_eq!(classify_reset(true, false, 99, false, false), ResetReason::OtherSoftReset);
        assert_eq!(classify_reset(true, false, 102, false, false), ResetReason::OtherSoftReset);
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
    fn is_init_stage_recognizes_known_stages() {
        assert!(is_init_stage(INIT_STAGE_WIFI));
        assert!(is_init_stage(INIT_STAGE_DHCP));
        assert!(!is_init_stage(0));
        assert!(!is_init_stage(1)); // an AppState discriminant
        assert!(!is_init_stage(99)); // just under the init range
        assert!(!is_init_stage(102)); // just past the init range
    }
}
