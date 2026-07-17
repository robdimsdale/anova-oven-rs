//! OTA firmware-update helpers.
//!
//! Wraps embassy-boot's `BlockingFirmwareUpdater` so callers (the
//! `/update_firmware` HTTP handler, the boot-time health gate) don't
//! have to know about flash peripherals, mutex wrappers, or
//! linker-symbol plumbing.
//!
//! Layout contract (must match `memory.x` exactly; bootloader's copy
//! at `crates/anova-oven-pico-bootloader/memory.x` must agree):
//!
//!   STATE @ 0x10008000, 4 KB
//!   ACTIVE @ 0x10009000, 988 KB    ← this binary
//!   DFU    @ 0x10100000, 1024 KB   ← OTA staging
//!
//! The DFU/STATE offsets are read from the `__bootloader_*` linker symbols
//! defined in `memory.x` (via `core::ptr::addr_of!`, no `unsafe` — see the
//! `partitions` module) and fed to `BlockingPartition` in [`make_config`].
//! We deliberately do *not* use
//! `FirmwareUpdaterConfig::from_linkerfile_blocking`: it fixes the mutex
//! type at the `!Sync` `NoopRawMutex`, which can't live in a static
//! without an unsafe impl. See [`make_config`] for the trade-off.

use core::cell::RefCell;

use defmt::warn;
use embassy_boot::FirmwareUpdaterError;
use embassy_boot_rp::{AlignedBuffer, BlockingFirmwareUpdater, FirmwareUpdaterConfig};
use embassy_embedded_hal::flash::partition::BlockingPartition;
use embassy_executor::Spawner;
use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::peripherals::FLASH;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::once_lock::OnceLock;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use portable_atomic::{AtomicBool, Ordering};

/// 2 MiB QSPI on the Pico W. Matches `bootloader/src/main.rs`.
pub const FLASH_SIZE: usize = 2 * 1024 * 1024;

/// Partition offsets/lengths sourced from the `__bootloader_*` linker
/// symbols in `memory.x` — the single source of truth for the layout,
/// shared (independently) with the bootloader's `memory.x`, which must
/// agree (a mismatch corrupts the new image's vector table on swap).
///
/// The linker assigns each symbol an *address* equal to its
/// FLASH_BASE-relative partition bound; the symbol has no contents, so we
/// only ever take its address. `addr_of!` computes that address without
/// loading from the extern static, so — unlike taking `&sym` or reading
/// `sym` — it needs no `unsafe`. Offsets are relative to `FLASH_BASE`
/// (0x10000000); embassy-rp's flash driver adds the base when accessing.
mod partitions {
    extern "C" {
        static __bootloader_state_start: u32;
        static __bootloader_state_end: u32;
        static __bootloader_dfu_start: u32;
        static __bootloader_dfu_end: u32;
    }

    #[inline]
    pub fn state_offset() -> u32 {
        core::ptr::addr_of!(__bootloader_state_start) as u32
    }
    #[inline]
    pub fn state_length() -> u32 {
        core::ptr::addr_of!(__bootloader_state_end) as u32 - state_offset()
    }
    #[inline]
    pub fn dfu_offset() -> u32 {
        core::ptr::addr_of!(__bootloader_dfu_start) as u32
    }
    #[inline]
    pub fn dfu_length() -> u32 {
        core::ptr::addr_of!(__bootloader_dfu_end) as u32 - dfu_offset()
    }
}

/// Size of the DFU partition. The `/update_firmware` handler rejects
/// POSTs larger than this with `413 Payload Too Large` before touching
/// the updater. Reads `__bootloader_dfu_*` at call time (a couple of
/// address loads — not worth caching).
#[inline]
pub fn dfu_partition_size() -> usize {
    partitions::dfu_length() as usize
}

/// Whole-chip blocking flash handle. Both DFU and STATE partition
/// reads/writes go through this single physical device —
/// `BlockingPartition` carves out the offsets at the
/// `BlockingFirmwareUpdater` level.
///
/// Wrapped in a `CriticalSectionRawMutex` (rather than the
/// `NoopRawMutex` baked into embassy-boot's `from_linkerfile_blocking`
/// convenience constructor) so the type is `Sync` and can live in a
/// static without any unsafe impls. The cost is a critical section
/// around each lock — negligible vs. the ~50 ms flash erase that
/// follows it, and the only contention is between the OTA handler
/// and the boot-time health gate (which never overlap).
type PicoFlash = Flash<'static, FLASH, Blocking, FLASH_SIZE>;
type FlashCell = Mutex<CriticalSectionRawMutex, RefCell<PicoFlash>>;

/// Set exactly once by [`install_flash`] at boot. `OnceLock::init`
/// enforces the singleton invariant at runtime — a second call returns
/// `Err` with the value, which [`install_flash`] turns into a panic.
static FLASH_LOCK: OnceLock<FlashCell> = OnceLock::new();

/// Guards [`mark_current_image_good`] so multiple successful API
/// exchanges don't repeatedly hit flash. The first call commits;
/// later calls short-circuit to `Ok(())`.
static MARKED_GOOD: AtomicBool = AtomicBool::new(false);

/// Fired by [`notify_api_success`] on the first successful API
/// exchange. Awaited by [`health_gate_task`], which then commits
/// the running image as good. `Signal` is already idempotent —
/// repeated fires before the waiter wakes merge into one wake.
static API_SUCCESS_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Called by the API task after each successful exchange. The first
/// call unblocks [`health_gate_task`], which marks the image good.
/// Subsequent calls are no-ops (the `Signal` coalesces them).
pub fn notify_api_success() {
    API_SUCCESS_SIGNAL.signal(());
}

/// Background task: waits for the first successful API exchange,
/// then confirms the running image so the bootloader keeps it.
/// Spawned once from `main` via [`spawn_health_gate_task`].
#[embassy_executor::task]
async fn health_gate_task() -> ! {
    API_SUCCESS_SIGNAL.wait().await;
    if let Err(e) = mark_current_image_good().await {
        warn!("ota health gate: mark_current_image_good failed: {}", e);
    }
    core::future::pending().await
}

/// Spawn [`health_gate_task`]. Call once at boot, after the embassy
/// executor is running.
pub fn spawn_health_gate_task(spawner: Spawner) {
    spawner.spawn(health_gate_task().unwrap());
}

#[derive(Debug)]
pub enum Error {
    /// `install_flash` was never called. Should be unreachable at
    /// runtime — surfaces a wiring bug if it ever fires.
    NotInitialized,
    Updater(FirmwareUpdaterError),
}

impl defmt::Format for Error {
    fn format(&self, f: defmt::Formatter<'_>) {
        match self {
            Error::NotInitialized => defmt::write!(f, "ota::Error::NotInitialized"),
            Error::Updater(e) => defmt::write!(f, "ota::Error::Updater({:?})", e),
        }
    }
}

/// Hand the FLASH peripheral over to the OTA layer for the lifetime
/// of the program. Must be called exactly once, early in `main`,
/// before any task that may push firmware or mark the image good. A
/// second call panics — that would indicate a real wiring bug.
pub fn install_flash(flash: PicoFlash) {
    if FLASH_LOCK
        .init(Mutex::new(RefCell::new(flash)))
        .is_err()
    {
        panic!("ota::install_flash called more than once");
    }
}

fn flash_ref() -> Result<&'static FlashCell, Error> {
    FLASH_LOCK.try_get().ok_or(Error::NotInitialized)
}

/// Build a `FirmwareUpdaterConfig` from the static flash cell using
/// hard-coded partition offsets. Avoids embassy-boot's
/// `from_linkerfile_blocking` convenience constructor, which fixes
/// the mutex type at the `!Sync` `NoopRawMutex`. The DFU/STATE
/// `BlockingPartition`s are `Copy`-cheap descriptors that hold a
/// borrow of the flash cell; the actual flash I/O happens later
/// inside `BlockingFirmwareUpdater::write_firmware` / `mark_*`.
fn make_config(
    flash: &FlashCell,
) -> FirmwareUpdaterConfig<
    BlockingPartition<'_, CriticalSectionRawMutex, PicoFlash>,
    BlockingPartition<'_, CriticalSectionRawMutex, PicoFlash>,
> {
    FirmwareUpdaterConfig {
        dfu: BlockingPartition::new(flash, partitions::dfu_offset(), partitions::dfu_length()),
        state: BlockingPartition::new(
            flash,
            partitions::state_offset(),
            partitions::state_length(),
        ),
    }
}

/// One in-flight OTA upload. Construction is free; the heavy lifting
/// happens in `write_chunk` and `finalize`. Intentionally not `Clone`/
/// `Copy` so the caller can't accidentally fan out concurrent writes
/// against the same DFU bank.
pub struct OtaSession {
    _private: (),
}

impl OtaSession {
    /// Begin a new upload. Idempotent — does no flash I/O, so calling
    /// it and dropping it without writes is harmless. `finalize` must
    /// be called to actually commit the staged image.
    pub fn begin() -> Self {
        Self { _private: () }
    }

    /// Write `data` at `offset` bytes from the start of the DFU
    /// partition. Erases the underlying 4 KiB sector when the write
    /// crosses a sector boundary. Blocks the executor for the
    /// duration of the flash op (~50 ms per sector on RP2040) —
    /// callers should `await` between chunks so the watchdog feeder
    /// gets time to run.
    pub fn write_chunk(&mut self, offset: u32, data: &[u8]) -> Result<(), Error> {
        // Construct a fresh updater per call. Cheap (no flash I/O on
        // construction) and avoids the self-referential-static
        // lifetime nightmare. The aligned scratch buffer must live on
        // the stack so concurrent constructions never alias —
        // embassy-boot writes it during state-partition reads.
        let flash = flash_ref()?;
        let mut aligned = AlignedBuffer([0u8; 4]);
        let mut updater = BlockingFirmwareUpdater::new(make_config(flash), &mut aligned.0);
        updater
            .write_firmware(offset as usize, data)
            .map_err(Error::Updater)
    }

    /// Mark the DFU bank ready for swap. The bootloader will perform
    /// the actual ACTIVE↔DFU copy on the next reset. After this
    /// returns, the next `reboot()` boots the new image; if anything
    /// fails before [`mark_current_image_good`] runs, the bootloader
    /// will roll back on the *subsequent* reset.
    pub fn finalize(&mut self) -> Result<(), Error> {
        let flash = flash_ref()?;
        let mut aligned = AlignedBuffer([0u8; 4]);
        let mut updater = BlockingFirmwareUpdater::new(make_config(flash), &mut aligned.0);
        updater.mark_updated().map_err(Error::Updater)
    }
}

/// Trigger an immediate system reset. Drops everything in RAM (the
/// persist region survives — see `persist.rs`).
pub fn reboot() -> ! {
    cortex_m::peripheral::SCB::sys_reset()
}

/// Signaled by the `/update_firmware` handler after the success
/// response has been written; awaited by [`reboot_task`].
/// `CriticalSectionRawMutex` rather than `NoopRawMutex` because the
/// signal lives in a static and needs `Sync`.
static REBOOT_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// How long to wait between [`request_reboot`] and the actual reset.
/// Gives picoserve / embassy-net time to flush the success response
/// onto the wire (and the operator's `curl` time to see `200 OK`)
/// before the kernel TCP buffers are wiped by reset.
const REBOOT_FLUSH_DELAY: Duration = Duration::from_millis(500);

/// Background task: waits for the OTA handler to signal completion,
/// gives TCP a moment to flush the response, then resets. Spawned
/// once from `main` so the handler can request a reboot without
/// blocking the response path.
#[embassy_executor::task]
async fn reboot_task() {
    REBOOT_SIGNAL.wait().await;
    Timer::after(REBOOT_FLUSH_DELAY).await;
    reboot()
}

/// Spawn [`reboot_task`]. Call once at boot, after the embassy
/// executor is running.
pub fn spawn_reboot_task(spawner: Spawner) {
    spawner.spawn(reboot_task().unwrap());
}

/// Schedule a reboot. Returns immediately; the actual reset fires
/// from [`reboot_task`] after [`REBOOT_FLUSH_DELAY`] so the caller
/// has time to flush any in-flight response.
pub fn request_reboot() {
    REBOOT_SIGNAL.signal(());
}

/// Confirm the currently running image is healthy enough that the
/// bootloader should keep it across the next reset. Called once after
/// DHCP completion *and* the first successful API exchange.
///
/// Idempotent: only the first call performs the state-partition
/// write; later calls short-circuit to `Ok(())` so the API task can
/// call it on every successful exchange without burning flash cycles.
///
/// `async` to match the plan's call site (`.await`d in `main.rs`);
/// the underlying `BlockingFirmwareUpdater::mark_booted` is sync, so
/// there's no real await inside.
pub async fn mark_current_image_good() -> Result<(), Error> {
    if MARKED_GOOD.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let flash = flash_ref()?;
    let mut aligned = AlignedBuffer([0u8; 4]);
    let mut updater = BlockingFirmwareUpdater::new(make_config(flash), &mut aligned.0);
    updater.mark_booted().map_err(Error::Updater)
}
