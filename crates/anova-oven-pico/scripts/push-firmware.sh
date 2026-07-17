#!/usr/bin/env bash
#
# Push a new firmware image to a running device over HTTP (OTA).
#
# Accepts an ELF or a raw binary. If an ELF is supplied it is converted to a
# flat binary first via arm-none-eabi-objcopy; binaries are pushed as-is.
#
# The device is expected to be running the app with the POST /update_firmware
# route active. It will return 200, stage the image, then reboot. The
# bootloader verifies the new image is marked good (see mark_booted() in
# ota.rs) before committing it; if the new image crashes before that point
# the bootloader rolls back automatically on the next reset.
#
# Usage:
#   scripts/push-firmware.sh <device-ip> [elf-or-bin]
#
# With only <device-ip>, the script auto-discovers the release app ELF.
#
# Examples:
#   scripts/push-firmware.sh 192.168.1.42
#   scripts/push-firmware.sh 192.168.1.42 \
#     ../../target/thumbv6m-none-eabi/release/anova-oven-pico
#   scripts/push-firmware.sh 192.168.1.42 firmware.bin
#
# Environment overrides:
#   PORT     HTTP port on the device              (default: 80)
#   PROFILE  cargo profile for auto-discovery     (default: release)
#
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 <device-ip> [elf-or-bin]" >&2
  exit 1
fi

DEVICE_IP="$1"
PORT="${PORT:-80}"
PROFILE="${PROFILE:-release}"
TARGET_TRIPLE="thumbv6m-none-eabi"
APP_BIN_NAME="anova-oven-pico"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(dirname "$SCRIPT_DIR")"
REPO_ROOT="$(git -C "$CRATE_DIR" rev-parse --show-toplevel)"

if [[ $# -eq 2 ]]; then
  INPUT="$2"
else
  INPUT=""
  for cand in \
    "$CRATE_DIR/target/$TARGET_TRIPLE/$PROFILE/$APP_BIN_NAME" \
    "$REPO_ROOT/target/$TARGET_TRIPLE/$PROFILE/$APP_BIN_NAME"; do
    if [[ -f "$cand" ]]; then
      INPUT="$cand"
      break
    fi
  done
fi

if [[ -z "${INPUT:-}" || ! -f "$INPUT" ]]; then
  echo "error: firmware file not found." >&2
  echo "       build it first: cargo build -p $APP_BIN_NAME --release" >&2
  echo "       or pass the path explicitly: $0 $DEVICE_IP <elf-or-bin>" >&2
  exit 1
fi

for tool in curl file; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: required tool '$tool' not found on PATH" >&2
    exit 1
  fi
done

BIN_FILE=""
TMPFILE=""

if file "$INPUT" | grep -q "ELF"; then
  if ! command -v arm-none-eabi-objcopy >/dev/null 2>&1; then
    echo "error: arm-none-eabi-objcopy not found on PATH" >&2
    echo "       install gcc-arm-none-eabi (macOS: brew install --cask gcc-arm-embedded)" >&2
    exit 1
  fi
  TMPFILE="$(mktemp /tmp/ota-firmware-XXXXXX.bin)"
  trap 'rm -f "$TMPFILE"' EXIT
  echo "==> Converting ELF → raw binary..."
  arm-none-eabi-objcopy -O binary "$INPUT" "$TMPFILE"
  BIN_FILE="$TMPFILE"
else
  BIN_FILE="$INPUT"
fi

SIZE="$(wc -c < "$BIN_FILE" | tr -d ' ')"
URL="http://${DEVICE_IP}:${PORT}/update_firmware"

echo "==> input:   $INPUT"
echo "==> binary:  $BIN_FILE  (${SIZE} bytes)"
echo "==> target:  $URL"
echo

RESP_FILE="$(mktemp /tmp/ota-response-XXXXXX.json)"
trap 'rm -f "$TMPFILE" "$RESP_FILE"' EXIT

HTTP_CODE="$(curl \
  --silent \
  --show-error \
  --write-out "%{http_code}" \
  --output "$RESP_FILE" \
  --data-binary "@${BIN_FILE}" \
  -H "Content-Type: application/octet-stream" \
  "$URL")"

if [[ "$HTTP_CODE" -eq 200 ]]; then
  echo "==> HTTP $HTTP_CODE — update staged. Device is rebooting."
  echo "    $(cat "$RESP_FILE")"
else
  echo "error: device returned HTTP $HTTP_CODE" >&2
  echo "       $(cat "$RESP_FILE")" >&2
  exit 1
fi
