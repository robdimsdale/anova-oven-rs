#!/usr/bin/env bash
#
# Provision a blank Pico W: flash the bootloader then the app via SWD.
#
# This is the one-time first-flash step. Subsequent updates should use
# scripts/push-firmware.sh (OTA over HTTP) instead.
#
# The flash layout this produces:
#
#   0x10000000  BOOT2 + bootloader code  (from bootloader ELF)
#   0x10009000  app (ACTIVE bank)        (from app ELF)
#
# After this script completes, cycle power or press RESET. The bootloader
# starts, finds no pending DFU image, and jumps to the app at 0x10009000.
#
# Usage:
#   scripts/flash-bootloader-and-app.sh [bootloader-elf [app-elf]]
#
# With no arguments, auto-discovers the default release builds in their
# respective crate target directories.
#
# Environment overrides:
#   CHIP      target chip for probe-rs          (default: RP2040)
#   PROTOCOL  probe-rs wire protocol            (default: swd)
#   PROFILE   cargo profile to look in          (default: release)
#
set -euo pipefail

CHIP="${CHIP:-RP2040}"
PROTOCOL="${PROTOCOL:-swd}"
PROFILE="${PROFILE:-release}"
TARGET_TRIPLE="thumbv6m-none-eabi"
BOOTLOADER_BIN_NAME="anova-oven-pico-bootloader"
APP_BIN_NAME="anova-oven-pico"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(dirname "$SCRIPT_DIR")"
REPO_ROOT="$(git -C "$CRATE_DIR" rev-parse --show-toplevel)"

BOOTLOADER_CRATE_DIR="$REPO_ROOT/crates/anova-oven-pico-bootloader"

if [[ $# -eq 2 ]]; then
  BOOTLOADER_ELF="$1"
  APP_ELF="$2"
elif [[ $# -eq 1 ]]; then
  echo "error: pass both a bootloader ELF and an app ELF, or neither" >&2
  exit 1
else
  BOOTLOADER_ELF=""
  APP_ELF=""

  for cand in \
    "$BOOTLOADER_CRATE_DIR/target/$TARGET_TRIPLE/$PROFILE/$BOOTLOADER_BIN_NAME" \
    "$REPO_ROOT/target/$TARGET_TRIPLE/$PROFILE/$BOOTLOADER_BIN_NAME"; do
    if [[ -f "$cand" ]]; then
      BOOTLOADER_ELF="$cand"
      break
    fi
  done

  for cand in \
    "$CRATE_DIR/target/$TARGET_TRIPLE/$PROFILE/$APP_BIN_NAME" \
    "$REPO_ROOT/target/$TARGET_TRIPLE/$PROFILE/$APP_BIN_NAME"; do
    if [[ -f "$cand" ]]; then
      APP_ELF="$cand"
      break
    fi
  done
fi

if [[ -z "${BOOTLOADER_ELF:-}" || ! -f "$BOOTLOADER_ELF" ]]; then
  echo "error: bootloader ELF not found." >&2
  echo "       build it first:" >&2
  echo "         (cd \"$BOOTLOADER_CRATE_DIR\" && cargo build --release)" >&2
  echo "       or pass the path explicitly: $0 <bootloader.elf> <app.elf>" >&2
  exit 1
fi

if [[ -z "${APP_ELF:-}" || ! -f "$APP_ELF" ]]; then
  echo "error: app ELF not found." >&2
  echo "       build it first: cargo build -p $APP_BIN_NAME --release" >&2
  echo "       or pass the path explicitly: $0 <bootloader.elf> <app.elf>" >&2
  exit 1
fi

if ! command -v probe-rs >/dev/null 2>&1; then
  echo "error: probe-rs not found on PATH" >&2
  exit 1
fi

echo "chip:        $CHIP   protocol: $PROTOCOL"
echo "bootloader:  $BOOTLOADER_ELF"
echo "app:         $APP_ELF"
echo

echo "==> Flashing bootloader (BOOT2 + bootloader code at 0x10000000)..."
probe-rs download --chip "$CHIP" --protocol "$PROTOCOL" "$BOOTLOADER_ELF"

echo "==> Flashing app (ACTIVE bank at 0x10009000)..."
probe-rs download --chip "$CHIP" --protocol "$PROTOCOL" "$APP_ELF"

echo
echo "==> Done. Both images are on flash."
echo "    Cycle power or press RESET to boot."
