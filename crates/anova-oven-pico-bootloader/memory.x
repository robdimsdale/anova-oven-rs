MEMORY
{
    BOOT2                             : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH                             : ORIGIN = 0x10000100, LENGTH = 32K - 0x100
    BOOTLOADER_STATE                  : ORIGIN = 0x10008000, LENGTH = 4K
    ACTIVE                            : ORIGIN = 0x10009000, LENGTH = 988K
    DFU                               : ORIGIN = 0x10100000, LENGTH = 1024K
    RAM                               : ORIGIN = 0x20000000, LENGTH = 264K
}

__bootloader_state_start = ORIGIN(BOOTLOADER_STATE) - ORIGIN(BOOT2);
__bootloader_state_end = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE) - ORIGIN(BOOT2);

__bootloader_active_start = ORIGIN(ACTIVE) - ORIGIN(BOOT2);
__bootloader_active_end = ORIGIN(ACTIVE) + LENGTH(ACTIVE) - ORIGIN(BOOT2);

__bootloader_dfu_start = ORIGIN(DFU) - ORIGIN(BOOT2);
__bootloader_dfu_end = ORIGIN(DFU) + LENGTH(DFU) - ORIGIN(BOOT2);
