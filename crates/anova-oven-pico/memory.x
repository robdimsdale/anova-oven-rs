/* OTA-partitioned flash layout.
 *
 * Must stay in sync with crates/anova-oven-pico-bootloader/memory.x.
 *
 *   0x10000000  +-----------------------------+
 *               | BOOT2 (256 B)               |  stage-2 — owned by bootloader
 *               +-----------------------------+
 *               | (bootloader code, 32 KB)    |  not visible to the app
 *               +-----------------------------+
 *   0x10008000  | BOOTLOADER_STATE (4 KB)     |  embassy-boot swap state
 *               +-----------------------------+
 *   0x10009000  | FLASH (988 KB) — ACTIVE     |  this binary lives here
 *               +-----------------------------+
 *   0x10100000  | DFU (1024 KB)               |  staging bank for incoming OTA
 *               +-----------------------------+
 *   0x10200000  end of flash (2 MB)
 *
 * The app does NOT include its own .boot2 — that lives in the
 * bootloader's image at 0x10000000 and is flashed once at provisioning
 * time. Pre-OTA monolithic builds used `EXTERN(BOOT2_FIRMWARE)` + a
 * `.boot2 ORIGIN(BOOT2)` SECTIONS block here; both are gone.
 *
 * The `__bootloader_*_start/end` symbols below are the FLASH_BASE-relative
 * partition bounds (computed as `ORIGIN(x) - ORIGIN(BOOT2)` because
 * embassy-rp's flash driver adds FLASH_BASE = 0x10000000 when accessing).
 * `ota.rs` reads their *addresses* with `core::ptr::addr_of!` — no `unsafe`,
 * no codegen — to build its `FirmwareUpdaterConfig`. They are the same
 * symbols `embassy-boot`'s `from_linkerfile*` reads; we read them ourselves
 * so the flash-cell mutex can stay `CriticalSectionRawMutex` (`Sync`, lives
 * in a static) instead of the `!Sync` `NoopRawMutex` that constructor forces.
 */

MEMORY
{
    BOOT2                : ORIGIN = 0x10000000, LENGTH = 0x100
    BOOTLOADER_STATE     : ORIGIN = 0x10008000, LENGTH = 4K
    FLASH                : ORIGIN = 0x10009000, LENGTH = 988K
    DFU                  : ORIGIN = 0x10100000, LENGTH = 1024K
    RAM                  : ORIGIN = 0x20000000, LENGTH = 264K
}

__bootloader_state_start = ORIGIN(BOOTLOADER_STATE) - ORIGIN(BOOT2);
__bootloader_state_end   = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE) - ORIGIN(BOOT2);

__bootloader_dfu_start   = ORIGIN(DFU) - ORIGIN(BOOT2);
__bootloader_dfu_end     = ORIGIN(DFU) + LENGTH(DFU) - ORIGIN(BOOT2);
