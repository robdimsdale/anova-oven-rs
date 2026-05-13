//! Persistent crash recording in a reserved SRAM region.
//!
//! On RP2040 the on-chip SRAM is preserved across all reset sources except
//! power-on, so we keep a small struct in `.uninit` (NOLOAD) for the most
//! recent panic message and reset counters. Validity is gated by a magic
//! word so we can detect a cold boot and discard the garbage that's there
//! when RAM has not been preserved.
//!
//! Lifecycle:
//!   - `init_at_boot()` runs once at startup. It validates the magic
//!     (zeroing the region on cold boot), increments `reset_count`, and
//!     returns a snapshot of what survived plus any stored message. The
//!     stored message is *consumed* — subsequent calls would see no
//!     message.
//!   - The `#[panic_handler]` and `HardFault` exception handler both write
//!     a message into the region, increment `panic_count`, and trigger a
//!     soft reset. The on-boot increment of `reset_count` happens on the
//!     next boot's `init_at_boot()` call (the panic path itself does not
//!     bump `reset_count`).
//!
//! We do not persist more than one panic — each panic overwrites the
//! previous message. Counters survive across soft/watchdog resets, but
//! reset on power-cycle (magic gets garbage and we re-init).

use core::fmt::Write;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use cortex_m::peripheral::SCB;
use cortex_m_rt::{exception, ExceptionFrame};

const MAGIC: u32 = 0xA9B0_C1D2;
const MSG_BUF_SIZE: usize = 512;

#[repr(C)]
struct PersistRegion {
    magic: u32,
    reset_count: u32,
    panic_count: u32,
    msg_len: u32,
    msg_buf: [u8; MSG_BUF_SIZE],
}

#[link_section = ".uninit.PERSIST"]
static mut PERSIST: MaybeUninit<PersistRegion> = MaybeUninit::uninit();

#[derive(Clone)]
pub struct Snapshot {
    pub reset_count: u32,
    pub panic_count: u32,
    pub message: Option<heapless::String<MSG_BUF_SIZE>>,
}

impl Snapshot {
    pub fn had_prior_failure(&self) -> bool {
        self.reset_count > 1 || self.panic_count > 0 || self.message.is_some()
    }
}

fn region_ptr() -> *mut PersistRegion {
    #[allow(static_mut_refs)]
    core::ptr::addr_of_mut!(PERSIST).cast::<PersistRegion>()
}

/// Returns true if the magic word matched (region was preserved). When this
/// returns false the caller is responsible for zero-initializing the region
/// and writing the magic.
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
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).msg_len), 0);
    }
}

/// Validate or initialize the region, bump `reset_count`, consume any
/// stored panic message, return a snapshot. Must be called exactly once
/// near the top of `main`.
pub fn init_at_boot() -> Snapshot {
    let ptr = region_ptr();
    let mut reset_count;
    let panic_count;
    let mut message: Option<heapless::String<MSG_BUF_SIZE>> = None;

    unsafe {
        if !magic_valid() {
            zero_region();
        }

        reset_count = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).reset_count));
        reset_count = reset_count.wrapping_add(1);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).reset_count), reset_count);

        panic_count = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).panic_count));

        let len = core::ptr::read_volatile(core::ptr::addr_of!((*ptr).msg_len)) as usize;
        if len > 0 && len <= MSG_BUF_SIZE {
            let mut bytes: heapless::Vec<u8, MSG_BUF_SIZE> = heapless::Vec::new();
            let bytes_ptr = core::ptr::addr_of!((*ptr).msg_buf) as *const u8;
            for i in 0..len {
                let _ = bytes.push(core::ptr::read_volatile(bytes_ptr.add(i)));
            }
            // Stored as UTF-8 by core::fmt::Write. If the buffer was
            // truncated mid-codepoint, trim to the last valid prefix.
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
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).msg_len), 0);
    }

    Snapshot {
        reset_count,
        panic_count,
        message,
    }
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

    // Best-effort defmt; in non-blocking mode this may be dropped if the
    // RTT buffer is full. The persisted message is the durable record.
    defmt::error!("panic recorded; resetting");

    // Memory barrier before reset so writes are visible after re-entry.
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
