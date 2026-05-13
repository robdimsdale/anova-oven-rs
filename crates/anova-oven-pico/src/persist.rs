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
//!   offset  0:  magic                       (u32)
//!   offset  4:  reset_count                 (u32)
//!   offset  8:  panic_count                 (u32)
//!   offset 12:  last_displayed_panic_count  (u32)
//!   offset 16:  reset_reason                (u32)  // ResetReason enum
//!   offset 20:  last_app_state              (u32)
//!   offset 24:  last_uptime_secs            (u32)
//!   offset 28:  api_heartbeat               (u32)
//!   offset 32:  display_heartbeat           (u32)
//!   offset 36:  watchdog_heartbeat          (u32)
//!   offset 40:  msg_len                     (u32)
//!   offset 44:  msg_buf                     ([u8; 512])
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

const MAGIC: u32 = 0xA9B0_C1D2;
const MSG_BUF_SIZE: usize = 512;

/// RP2040 WATCHDOG.REASON register. Bit 0 = TIMER (timed out), bit 1 =
/// FORCE (TRIGGER bit set in CTRL). Bits clear when the watchdog is
/// enabled, so we must read this before `Watchdog::start()`.
const WATCHDOG_REASON_ADDR: *const u32 = 0x4005_8008 as *const u32;

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
    msg_len: u32,
    msg_buf: [u8; MSG_BUF_SIZE],
}

#[link_section = ".uninit.PERSIST"]
static mut PERSIST: MaybeUninit<PersistRegion> = MaybeUninit::uninit();

/// What caused the boot we're currently in.
#[derive(Copy, Clone, defmt::Format)]
#[repr(u32)]
#[allow(dead_code)] // Unknown is observable via probe-rs before init_at_boot runs.
pub enum ResetReason {
    /// Reserved for invalid stored values. Should never appear in a
    /// freshly-computed snapshot.
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
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).msg_len), 0);
    }
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
fn classify_reset(magic_was_valid: bool, panic_count_advanced: bool) -> ResetReason {
    let (timer, force) = read_watchdog_reason_raw();
    if !magic_was_valid {
        ResetReason::ColdBoot
    } else if timer {
        ResetReason::WatchdogTimeout
    } else if force {
        ResetReason::WatchdogForced
    } else if panic_count_advanced {
        ResetReason::Panic
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
    let reset_reason = classify_reset(magic_was_valid, message_is_new);

    // Persist the classified reason so it's readable via probe-rs at any
    // point in this boot's lifetime. (init_at_boot is the only writer.)
    unsafe {
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*ptr).reset_reason),
            reset_reason as u32,
        );
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

/// Update the "uptime at last successful watchdog feed" breadcrumb.
pub fn record_uptime_secs(secs: u32) {
    let ptr = region_ptr();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_uptime_secs), secs);
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

    defmt::error!("panic recorded; resetting");

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
