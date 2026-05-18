//! Persistent crash recording in a reserved SRAM region.
//!
//! On RP2040 the on-chip SRAM is preserved across all reset sources except
//! power-on, so we keep a small struct in `.uninit` (NOLOAD) for the most
//! recent panic message, reset counters, and live "breadcrumbs" updated
//! by tasks while the device is running. Validity is gated by a magic
//! word so we can detect a cold boot and discard garbage RAM contents.
//!
//! Layout (`#[repr(C)]`, all fields naturally u32-aligned):
//!
//!   offset   0:  magic                       (u32)
//!   offset   4:  reset_count                 (u32)
//!   offset   8:  panic_count                 (u32)
//!   offset  12:  last_displayed_panic_count  (u32)
//!   offset  16:  reset_reason                (u32)  // ResetReason enum
//!   offset  20:  last_app_state              (u32)
//!   offset  24:  last_uptime_secs            (u32)
//!   offset  28:  api_heartbeat               (u32)
//!   offset  32:  display_heartbeat           (u32)
//!   offset  36:  watchdog_heartbeat          (u32)
//!   offset  40:  last_free_heap              (u32)  // bytes, by feeder
//!   offset  44:  network_up                  (u32)  // 0/1, this run
//!   offset  48:  last_api_fail_count         (u32)
//!   offset  52:  ring_head                   (u32)  // monotonic; index = head % RING_SIZE
//!   offset  56:  ring                        ([RingEntry; 8] = 192 bytes)
//!   offset 248:  msg_len                     (u32)
//!   offset 252:  msg_buf                     ([u8; 512])
//!
//! Each `RingEntry` (6x u32 = 24 bytes) describes one reset that
//! happened *before* the boot that wrote the entry: the reset reason,
//! how long the run lasted, and the run's last-known api_heartbeat,
//! free heap, network-up flag and api fail-count. The ring stores the
//! most recent RING_SIZE resets; older ones are overwritten. The
//! current boot's reason lives in the standalone `reset_reason` field
//! at offset 16 (and is also the head of the ring after `init_at_boot`
//! runs).
//!
//! When this layout changes, bump `MAGIC` so old in-RAM data fails the
//! magic check after a firmware update and we re-initialize cleanly
//! instead of decoding stale bytes against the new field offsets.
//!
//! Tasks update their heartbeat fields directly via raw volatile writes
//! (u32 writes are atomic on Cortex-M0+; no critical section needed).
//! On boot, `init_at_boot()` reads everything into a `Snapshot` for
//! logging and decides whether to flash the LCD recovery view.
//!
//! The message buffer is NOT cleared on boot — it stays readable via
//! probe-rs until the next panic overwrites it.

use core::fmt::Write;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use cortex_m::peripheral::SCB;
use cortex_m_rt::{exception, ExceptionFrame};

// Bump this when changing the PersistRegion layout. Old in-RAM data
// from before the firmware update will then fail the magic check and we
// re-initialize cleanly instead of mis-decoding stale bytes.
//   v1 = 0xA9B0_C1D2 — original layout, no ring buffer
//   v2 = 0xA9B0_C1D3 — added ring_head + ring (8 entries, 2 u32 each)
//   v3 = 0xA9B0_C1D4 — added last_free_heap/network_up/last_api_fail_count
//                      live fields; RingEntry grown to 6 u32
const MAGIC: u32 = 0xA9B0_C1D4;
const MSG_BUF_SIZE: usize = 512;
const RING_SIZE: usize = 8;

/// RP2040 WATCHDOG.REASON register. Bit 0 = TIMER (timed out), bit 1 =
/// FORCE (TRIGGER bit set in CTRL). Bits clear when the watchdog is
/// enabled, so we must read this before `Watchdog::start()`.
const WATCHDOG_REASON_ADDR: *const u32 = 0x4005_8008 as *const u32;

#[repr(C)]
#[derive(Copy, Clone)]
struct RingEntry {
    reset_reason: u32,
    uptime_secs: u32,
    api_heartbeat: u32,
    free_heap: u32,
    network_up: u32,
    api_fail_count: u32,
}

const RING_ENTRY_WORDS: usize = 6;

#[repr(C)]
struct PersistRegion {
    magic: u32,
    reset_count: u32,
    panic_count: u32,
    last_displayed_panic_count: u32,
    reset_reason: u32,
    last_app_state: u32,
    last_uptime_secs: u32,
    api_heartbeat: u32,
    display_heartbeat: u32,
    watchdog_heartbeat: u32,
    last_free_heap: u32,
    network_up: u32,
    last_api_fail_count: u32,
    ring_head: u32,
    ring: [RingEntry; RING_SIZE],
    msg_len: u32,
    msg_buf: [u8; MSG_BUF_SIZE],
}

#[link_section = ".uninit.PERSIST"]
static mut PERSIST: MaybeUninit<PersistRegion> = MaybeUninit::uninit();

/// What caused the boot we're currently in.
#[derive(Copy, Clone, defmt::Format)]
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
    /// `INIT_STAGE_*` value at the next boot (see `classify_reset`).
    InitTimeout = 6,
}

/// `last_app_state` breadcrumb values for the pre-`AppState` bring-up
/// phases. Deliberately well clear of `AppState::discriminant()` (1..=8)
/// so a reset *during* init is distinguishable from one in the running
/// state machine. `main` records these before each bring-up wait; the
/// state machine overwrites `last_app_state` with its own discriminants
/// once it starts, so a non-init value means we got past bring-up.
pub const INIT_STAGE_WIFI: u32 = 100;
pub const INIT_STAGE_DHCP: u32 = 101;

fn is_init_stage(s: u32) -> bool {
    matches!(s, INIT_STAGE_WIFI | INIT_STAGE_DHCP)
}

impl ResetReason {
    fn from_u32(v: u32) -> Self {
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

#[derive(Copy, Clone, defmt::Format)]
pub struct ResetHistoryEntry {
    pub reset_reason: ResetReason,
    pub uptime_secs: u32,
    pub api_heartbeat: u32,
    pub free_heap: u32,
    pub network_up: bool,
    pub api_fail_count: u32,
}

#[derive(Clone)]
pub struct Snapshot {
    pub reset_count: u32,
    pub panic_count: u32,
    pub message: Option<heapless::String<MSG_BUF_SIZE>>,
    /// True when `panic_count` advanced past `last_displayed_panic_count`
    /// — i.e. a panic happened since the last LCD recovery view.
    pub message_is_new: bool,
    pub reset_reason: ResetReason,
    pub last_app_state: u32,
    pub last_uptime_secs: u32,
    pub api_heartbeat: u32,
    pub display_heartbeat: u32,
    pub watchdog_heartbeat: u32,
    /// The last RING_SIZE resets, newest first. Includes the reset that
    /// produced *this* boot at index 0 (unless it was a cold boot).
    pub reset_history: heapless::Vec<ResetHistoryEntry, RING_SIZE>,
}

fn region_ptr() -> *mut PersistRegion {
    #[allow(static_mut_refs)]
    core::ptr::addr_of_mut!(PERSIST).cast::<PersistRegion>()
}

unsafe fn magic_valid() -> bool {
    let ptr = region_ptr();
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*ptr).magic)) == MAGIC }
}

unsafe fn zero_region() {
    let ptr = region_ptr();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).magic), MAGIC);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).reset_count), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).panic_count), 0);
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*ptr).last_displayed_panic_count),
            0,
        );
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).reset_reason), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_app_state), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_uptime_secs), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).api_heartbeat), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).display_heartbeat), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).watchdog_heartbeat), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_free_heap), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).network_up), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_api_fail_count), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).ring_head), 0);
        let ring_ptr = core::ptr::addr_of_mut!((*ptr).ring) as *mut u32;
        for i in 0..(RING_SIZE * RING_ENTRY_WORDS) {
            core::ptr::write_volatile(ring_ptr.add(i), 0);
        }
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).msg_len), 0);
    }
}

/// Append one entry to the ring buffer. `ring_head` is a monotonic
/// counter; the actual slot is `ring_head % RING_SIZE`. The non-reason
/// fields are snapshotted from the live persist fields (i.e. the last
/// values the run that just ended managed to write).
unsafe fn ring_append(reason: ResetReason, uptime_secs: u32) {
    let ptr = region_ptr();
    unsafe {
        let api_heartbeat = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).api_heartbeat));
        let free_heap = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).last_free_heap));
        let network_up = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).network_up));
        let api_fail_count =
            core::ptr::read_volatile(core::ptr::addr_of!((*ptr).last_api_fail_count));

        let head = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).ring_head));
        let idx = (head as usize) % RING_SIZE;
        let entry_ptr = core::ptr::addr_of_mut!((*ptr).ring[idx]);
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*entry_ptr).reset_reason),
            reason as u32,
        );
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*entry_ptr).uptime_secs),
            uptime_secs,
        );
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*entry_ptr).api_heartbeat),
            api_heartbeat,
        );
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*entry_ptr).free_heap), free_heap);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*entry_ptr).network_up), network_up);
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*entry_ptr).api_fail_count),
            api_fail_count,
        );
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*ptr).ring_head),
            head.wrapping_add(1),
        );
    }
}

/// Read the ring buffer in newest-first order. Returns up to RING_SIZE
/// entries (fewer if the ring hasn't been filled yet).
unsafe fn ring_read() -> heapless::Vec<ResetHistoryEntry, RING_SIZE> {
    let ptr = region_ptr();
    let mut out: heapless::Vec<ResetHistoryEntry, RING_SIZE> = heapless::Vec::new();
    unsafe {
        let head = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).ring_head)) as usize;
        let count = head.min(RING_SIZE);
        for i in 0..count {
            // i=0 → most recent (head - 1), i=1 → head - 2, ...
            let idx = (head + RING_SIZE - 1 - i) % RING_SIZE;
            let entry_ptr = core::ptr::addr_of!((*ptr).ring[idx]);
            let reason_raw =
                core::ptr::read_volatile(core::ptr::addr_of!((*entry_ptr).reset_reason));
            let uptime_secs =
                core::ptr::read_volatile(core::ptr::addr_of!((*entry_ptr).uptime_secs));
            let api_heartbeat =
                core::ptr::read_volatile(core::ptr::addr_of!((*entry_ptr).api_heartbeat));
            let free_heap = core::ptr::read_volatile(core::ptr::addr_of!((*entry_ptr).free_heap));
            let network_up =
                core::ptr::read_volatile(core::ptr::addr_of!((*entry_ptr).network_up)) != 0;
            let api_fail_count =
                core::ptr::read_volatile(core::ptr::addr_of!((*entry_ptr).api_fail_count));
            let _ = out.push(ResetHistoryEntry {
                reset_reason: ResetReason::from_u32(reason_raw),
                uptime_secs,
                api_heartbeat,
                free_heap,
                network_up,
                api_fail_count,
            });
        }
    }
    out
}

/// Read WATCHDOG.REASON directly from MMIO. Doesn't require ownership of
/// the peripheral. The register's bits clear on `Watchdog::start()`, so
/// we must call this *before* the watchdog is enabled.
fn read_watchdog_reason_raw() -> (bool, bool) {
    let raw = unsafe { core::ptr::read_volatile(WATCHDOG_REASON_ADDR) };
    let timer = (raw & 0b01) != 0;
    let force = (raw & 0b10) != 0;
    (timer, force)
}

/// Combine WATCHDOG.REASON with our persist state to decide what kind of
/// reset just happened. Called only by `init_at_boot()`.
///
/// A panic is checked *before* the watchdog timer bit on purpose: a
/// panic is the root cause, and if a slow/hung panic handler lets the
/// watchdog fire too (WATCHDOG.REASON.TIMER set), we still want the
/// reset attributed to the panic rather than masked as a plain
/// watchdog timeout.
///
/// `InitTimeout` is what would otherwise be an `OtherSoftReset` (plain
/// soft reset, no panic, no watchdog) but with `last_app_state` still in
/// the `INIT_STAGE_*` range — i.e. the box reset while in WiFi/DHCP
/// bring-up, which is our deliberate `reboot_init_timeout()`. (A stray
/// NRST during bring-up lands here too; "reset during init" is a fair
/// label either way.)
fn classify_reset(
    magic_was_valid: bool,
    panic_count_advanced: bool,
    last_app_state: u32,
) -> ResetReason {
    let (timer, force) = read_watchdog_reason_raw();
    if !magic_was_valid {
        ResetReason::ColdBoot
    } else if panic_count_advanced {
        ResetReason::Panic
    } else if timer {
        ResetReason::WatchdogTimeout
    } else if force {
        ResetReason::WatchdogForced
    } else if is_init_stage(last_app_state) {
        ResetReason::InitTimeout
    } else {
        ResetReason::OtherSoftReset
    }
}

/// Validate or initialize the region, bump `reset_count`, read all
/// breadcrumb fields and any stored panic message, classify the reset,
/// and return a snapshot. Must be called once near the top of `main`
/// *before* `Watchdog::start()` (so WATCHDOG.REASON is still legible).
/// The message buffer is left intact so probe-rs can read it at any
/// later point.
pub fn init_at_boot() -> Snapshot {
    let ptr = region_ptr();
    let mut reset_count;
    let panic_count;
    let last_displayed_panic_count;
    let last_app_state;
    let last_uptime_secs;
    let api_heartbeat;
    let display_heartbeat;
    let watchdog_heartbeat;
    let magic_was_valid;
    let mut message: Option<heapless::String<MSG_BUF_SIZE>> = None;

    unsafe {
        magic_was_valid = magic_valid();
        if !magic_was_valid {
            zero_region();
        }

        reset_count = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).reset_count));
        reset_count = reset_count.wrapping_add(1);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).reset_count), reset_count);

        panic_count = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).panic_count));
        last_displayed_panic_count =
            core::ptr::read_volatile(core::ptr::addr_of!((*ptr).last_displayed_panic_count));
        last_app_state = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).last_app_state));
        last_uptime_secs = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).last_uptime_secs));
        api_heartbeat = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).api_heartbeat));
        display_heartbeat = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).display_heartbeat));
        watchdog_heartbeat =
            core::ptr::read_volatile(core::ptr::addr_of!((*ptr).watchdog_heartbeat));

        let len = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).msg_len)) as usize;
        if len > 0 && len <= MSG_BUF_SIZE {
            let mut bytes: heapless::Vec<u8, MSG_BUF_SIZE> = heapless::Vec::new();
            let bytes_ptr = core::ptr::addr_of!((*ptr).msg_buf) as *const u8;
            for i in 0..len {
                let _ = bytes.push(core::ptr::read_volatile(bytes_ptr.add(i)));
            }
            let mut s: heapless::String<MSG_BUF_SIZE> = heapless::String::new();
            let valid_end = match core::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(e) => e.valid_up_to(),
            };
            if let Ok(prefix) = core::str::from_utf8(&bytes[..valid_end]) {
                let _ = s.push_str(prefix);
            }
            message = Some(s);
        }
    }

    let message_is_new = panic_count > last_displayed_panic_count;
    let reset_reason = classify_reset(magic_was_valid, message_is_new, last_app_state);

    // Persist the classified reason so it's readable via probe-rs at any
    // point in this boot's lifetime. (init_at_boot is the only writer.)
    unsafe {
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*ptr).reset_reason),
            reset_reason as u32,
        );
    }

    // Append this boot's reset to the ring buffer (excluding cold boot —
    // there's no meaningful "previous run" duration to record). The
    // uptime we log is the previous run's `last_uptime_secs`, i.e. how
    // long the run that just ended had been alive at its last watchdog
    // feed.
    if !matches!(reset_reason, ResetReason::ColdBoot) {
        unsafe {
            ring_append(reset_reason, last_uptime_secs);
        }
    }
    let reset_history = unsafe { ring_read() };

    // Reset per-run breadcrumbs now that the previous run's values have
    // been snapshotted into the ring. These describe the *current* run
    // and must start clean so a freeze before the network comes up (or
    // before the first feed) is distinguishable from one after.
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).network_up), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_api_fail_count), 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_free_heap), 0);
    }

    Snapshot {
        reset_count,
        panic_count,
        message,
        message_is_new,
        reset_reason,
        last_app_state,
        last_uptime_secs,
        api_heartbeat,
        display_heartbeat,
        watchdog_heartbeat,
        reset_history,
    }
}

/// Advance `last_displayed_panic_count` to the current `panic_count` so
/// subsequent boots don't re-flash the same recovery message. Call this
/// after the LCD recovery view has finished.
pub fn mark_displayed() {
    let ptr = region_ptr();
    unsafe {
        if !magic_valid() {
            return;
        }
        let n = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).panic_count));
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*ptr).last_displayed_panic_count),
            n,
        );
    }
}

/// Update the "last app state" breadcrumb. Called from `AppState::execute`.
pub fn record_app_state(discriminant: u32) {
    let ptr = region_ptr();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_app_state), discriminant);
    }
}

/// Deliberately reboot because a bring-up stage (WiFi join / DHCP)
/// didn't complete within its deadline. Caller must have set the
/// `last_app_state` breadcrumb to the relevant `INIT_STAGE_*` value
/// (via `record_app_state`) *before* the wait, so that this plain soft
/// reset is attributed to `ResetReason::InitTimeout` next boot and the
/// reset ring records which stage stalled. Never returns.
///
/// Safe to call from normal async context (unlike the panic path, this
/// isn't re-entrancy-sensitive — the caller should `warn!` first).
pub fn reboot_init_timeout() -> ! {
    cortex_m::asm::dsb();
    SCB::sys_reset();
}

/// Update the "uptime at last successful watchdog feed" breadcrumb.
pub fn record_uptime_secs(secs: u32) {
    let ptr = region_ptr();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_uptime_secs), secs);
    }
}

/// Update the "free heap at last watchdog feed" breadcrumb. Called from
/// the watchdog feeder so it's recorded regardless of the verbose-logs
/// feature (the heap monitor task is gated behind it).
pub fn record_free_heap(bytes: u32) {
    let ptr = region_ptr();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_free_heap), bytes);
    }
}

/// Mark that the network came up during this run. Called once from
/// `main` after DHCP completes. Reset to 0 each boot by `init_at_boot`.
pub fn record_network_up() {
    let ptr = region_ptr();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).network_up), 1);
    }
}

/// Record the API client's current consecutive fail count. Called from
/// `api_client` whenever it changes. Reset to 0 each boot.
pub fn record_api_fail_count(n: u32) {
    let ptr = region_ptr();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_api_fail_count), n);
    }
}

fn bump(field: *mut u32) {
    unsafe {
        let v = core::ptr::read_volatile(field).wrapping_add(1);
        core::ptr::write_volatile(field, v);
    }
}

pub fn bump_api_heartbeat() {
    let ptr = region_ptr();
    bump(unsafe { core::ptr::addr_of_mut!((*ptr).api_heartbeat) });
}

pub fn bump_display_heartbeat() {
    let ptr = region_ptr();
    bump(unsafe { core::ptr::addr_of_mut!((*ptr).display_heartbeat) });
}

pub fn bump_watchdog_heartbeat() {
    let ptr = region_ptr();
    bump(unsafe { core::ptr::addr_of_mut!((*ptr).watchdog_heartbeat) });
}

/// Best-effort writer into the panic-message buffer. Truncates silently.
struct MsgWriter {
    pos: usize,
}

impl Write for MsgWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let ptr = region_ptr();
        let bytes = s.as_bytes();
        let remaining = MSG_BUF_SIZE.saturating_sub(self.pos);
        let n = bytes.len().min(remaining);
        unsafe {
            let buf_ptr = core::ptr::addr_of_mut!((*ptr).msg_buf) as *mut u8;
            for (i, b) in bytes.iter().take(n).enumerate() {
                core::ptr::write_volatile(buf_ptr.add(self.pos + i), *b);
            }
        }
        self.pos += n;
        Ok(())
    }
}

/// Common path for both `#[panic_handler]` and the HardFault exception:
/// ensure magic, bump panic_count, format `args` into the message buffer,
/// then soft-reset. Never returns.
fn record_and_reset(args: core::fmt::Arguments) -> ! {
    cortex_m::interrupt::disable();

    let ptr = region_ptr();
    unsafe {
        if !magic_valid() {
            zero_region();
        }
        let n = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).panic_count));
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*ptr).panic_count),
            n.wrapping_add(1),
        );

        let mut writer = MsgWriter { pos: 0 };
        let _ = writer.write_fmt(args);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).msg_len), writer.pos as u32);
    }

    // Deliberately NO defmt here. If the panic interrupted code holding
    // defmt's global logger, calling into defmt now re-enters it and
    // double-panics, hanging the handler until the watchdog fires (which
    // mis-attributes the reset as a watchdog timeout). The persisted
    // message is the durable record; init_at_boot logs it next boot.
    cortex_m::asm::dsb();

    SCB::sys_reset();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    record_and_reset(format_args!("{}", info))
}

#[exception]
unsafe fn HardFault(frame: &ExceptionFrame) -> ! {
    record_and_reset(format_args!(
        "HARDFAULT pc=0x{:08x} lr=0x{:08x} r0=0x{:08x} r1=0x{:08x} r2=0x{:08x} r3=0x{:08x} xpsr=0x{:08x}",
        frame.pc(),
        frame.lr(),
        frame.r0(),
        frame.r1(),
        frame.r2(),
        frame.r3(),
        frame.xpsr(),
    ))
}
