#!/usr/bin/env bash
#
# Dump and decode the firmware's persistent crash-recording region.
#
# The anova-oven-pico firmware keeps a `PersistRegion` struct in a
# reserved, NOLOAD SRAM section (`.uninit.PERSIST`) that survives every
# reset except power-on. See crates/anova-oven-pico/src/persist.rs for
# the authoritative layout — this script must be kept in sync with it
# (and with the `MAGIC` value when the layout changes).
#
# It locates the static's address with `arm-none-eabi-nm`, reads the
# region over SWD with `probe-rs`, then decodes it the same way
# `init_at_boot()` / `ring_read()` do.
#
# Usage:
#   scripts/dump-persist.sh [path/to/elf]
#
# Environment overrides:
#   CHIP     target chip for probe-rs   (default: RP2040)
#   PROFILE  cargo profile to look in   (default: release)
#   BIN      explicit ELF path          (overrides auto-detection)
#
set -euo pipefail

CHIP="${CHIP:-RP2040}"
PROFILE="${PROFILE:-release}"
TARGET_TRIPLE="thumbv6m-none-eabi"
BIN_NAME="anova-oven-pico"

# Resolve key directories independent of where we're invoked from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(dirname "$SCRIPT_DIR")"
REPO_ROOT="$(git -C "$CRATE_DIR" rev-parse --show-toplevel)"

# Find the ELF: explicit arg, then $BIN, then the usual cargo locations.
# A workspace build lands in the repo-root target/; a standalone build
# in the crate may use the crate-local target/.
ELF=""
if [[ $# -ge 1 ]]; then
  ELF="$1"
elif [[ -n "${BIN:-}" ]]; then
  ELF="$BIN"
else
  for cand in \
    "$REPO_ROOT/target/$TARGET_TRIPLE/$PROFILE/$BIN_NAME" \
    "$CRATE_DIR/target/$TARGET_TRIPLE/$PROFILE/$BIN_NAME"; do
    if [[ -f "$cand" ]]; then
      ELF="$cand"
      break
    fi
  done
fi

if [[ -z "$ELF" || ! -f "$ELF" ]]; then
  echo "error: could not find the $PROFILE ELF for $BIN_NAME." >&2
  echo "       build it first (cargo build -p $BIN_NAME --$PROFILE)" >&2
  echo "       or pass the path explicitly: $0 path/to/elf" >&2
  exit 1
fi

for tool in arm-none-eabi-nm probe-rs python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: required tool '$tool' not found on PATH" >&2
    exit 1
  fi
done

# The static's mangled symbol ends in `...7persist7PERSIST`; the crate
# hash in the middle changes between builds, so match the stable tail.
ADDR="$(arm-none-eabi-nm "$ELF" \
  | awk '$3 ~ /7persist7PERSIST$/ { print $1; exit }')"

if [[ -z "$ADDR" ]]; then
  echo "error: PERSIST symbol not found in $ELF" >&2
  exit 1
fi

# Pull the layout constants straight out of persist.rs instead of
# duplicating them here, so the script can't silently drift from the
# firmware it's decoding. Rust int literals may contain '_' separators.
PERSIST_RS="$CRATE_DIR/src/persist.rs"
if [[ ! -f "$PERSIST_RS" ]]; then
  echo "error: cannot find $PERSIST_RS to read layout constants" >&2
  exit 1
fi

rs_const() { # name -> literal (underscores stripped), via the `= <lit>;`
    sed -nE "s/^[[:space:]]*const $1:[^=]*=[[:space:]]*([0-9A-Fa-fx_]+)[[:space:]]*;.*/\1/p" \
    "$PERSIST_RS" | head -n1 | tr -d '_'
}

MAGIC_RS="$(rs_const MAGIC)"
RING_SIZE_RS="$(rs_const RING_SIZE)"
MSG_BUF_SIZE_RS="$(rs_const MSG_BUF_SIZE)"

if [[ -z "$MAGIC_RS" || -z "$RING_SIZE_RS" || -z "$MSG_BUF_SIZE_RS" ]]; then
  echo "error: failed to parse MAGIC/RING_SIZE/MSG_BUF_SIZE from $PERSIST_RS" >&2
  exit 1
fi

# Region size in u32 words, derived from the parsed constants so it
# tracks layout changes too:
#   11 fixed header words (magic .. ring_head)
# + RING_SIZE * 2          (ring of (reason, uptime) pairs)
# + 1                      (msg_len)
# + MSG_BUF_SIZE / 4       (msg_buf bytes)
WORDS=$(( 11 + RING_SIZE_RS * 2 + 1 + MSG_BUF_SIZE_RS / 4 ))

echo "ELF:     $ELF"
echo "symbol:  0x$ADDR (.uninit.PERSIST)"
echo "chip:    $CHIP"
echo "layout:  MAGIC=$MAGIC_RS RING_SIZE=$RING_SIZE_RS" \
     "MSG_BUF_SIZE=$MSG_BUF_SIZE_RS -> $WORDS words (from persist.rs)"
echo

RAW="$(probe-rs read --chip "$CHIP" b32 "0x$ADDR" "$WORDS")"

# Hand the flat list of hex words to python for the actual decode. The
# decode *logic* still mirrors persist.rs; the constants are sourced
# from it above and passed through the environment.
export MAGIC_RS RING_SIZE_RS MSG_BUF_SIZE_RS
echo "$RAW" | python3 - "$WORDS" <<'PY'
import os
import sys

expected = int(sys.argv[1])
words = []
for tok in sys.stdin.read().split():
    words.append(int(tok, 16))

if len(words) < expected:
    sys.exit(f"error: expected {expected} words from probe-rs, got {len(words)}")

MAGIC = int(os.environ["MAGIC_RS"], 0)              # from persist.rs
RING_SIZE = int(os.environ["RING_SIZE_RS"], 0)      # from persist.rs
MSG_BUF_SIZE = int(os.environ["MSG_BUF_SIZE_RS"], 0)  # from persist.rs

RESET_REASON = {
    0: "Unknown",
    1: "ColdBoot",
    2: "Panic",
    3: "WatchdogTimeout",
    4: "WatchdogForced",
    5: "OtherSoftReset",
}


def reason(v):
    return RESET_REASON.get(v, f"Unknown({v})")


def hms(secs):
    h, rem = divmod(secs, 3600)
    m, s = divmod(rem, 60)
    if h:
        return f"{secs}s (~{h}h {m}m {s}s)"
    if m:
        return f"{secs}s (~{m}m {s}s)"
    return f"{secs}s"


magic = words[0]
reset_count = words[1]
panic_count = words[2]
last_displayed = words[3]
reset_reason = words[4]
last_app_state = words[5]
last_uptime = words[6]
api_hb = words[7]
display_hb = words[8]
watchdog_hb = words[9]
ring_head = words[10]
# Offsets derived from the constants, not hardcoded: ring is RING_SIZE
# (reason, uptime) pairs starting at word 11, then msg_len, then the
# MSG_BUF_SIZE-byte msg_buf.
ring_start = 11
ring = words[ring_start:ring_start + RING_SIZE * 2]
msg_len_idx = ring_start + RING_SIZE * 2
msg_len = words[msg_len_idx]
msg_words = words[msg_len_idx + 1:msg_len_idx + 1 + MSG_BUF_SIZE // 4]

if magic != MAGIC:
    print(f"magic:   0x{magic:08x}  ** MISMATCH ** "
          f"(expected 0x{MAGIC:08x})")
    print()
    print("Region is invalid: either a cold boot since power-on, RAM was")
    print("lost, or this script's MAGIC is stale vs. the running firmware.")
    print("Decoded fields below are NOT trustworthy.")
    print()

print(f"magic:                       0x{magic:08x}"
      f"{'  (valid)' if magic == MAGIC else ''}")
print(f"reset_count:                 {reset_count}")
print(f"panic_count:                 {panic_count}")
print(f"last_displayed_panic_count:  {last_displayed}")
message_is_new = panic_count > last_displayed
print(f"reset_reason (this boot):    {reset_reason} = {reason(reset_reason)}")
print(f"last_app_state:              {last_app_state}")
print(f"last_uptime_secs:            {hms(last_uptime)}")
print(f"api_heartbeat:               {api_hb}")
print(f"display_heartbeat:           {display_hb}")
print(f"watchdog_heartbeat:          {watchdog_hb}")
print(f"ring_head:                   {ring_head}")
print(f"msg_len:                     {msg_len}")
print(f"message_is_new:              {message_is_new}")

# Replicate ring_read(): newest first, idx = (head + RING_SIZE - 1 - i) %
# RING_SIZE, for i in 0..min(head, RING_SIZE).
print()
count = min(ring_head, RING_SIZE)
if count == 0:
    print("reset history: (empty)")
else:
    print(f"reset history (newest first, {count} of max {RING_SIZE}):")
    for i in range(count):
        idx = (ring_head + RING_SIZE - 1 - i) % RING_SIZE
        r = ring[idx * 2]
        up = ring[idx * 2 + 1]
        tag = "  <- produced this boot" if i == 0 else ""
        print(f"  [{i}] {reason(r):<16} prev run uptime {hms(up)}{tag}")

# Decode the stored panic message: msg_buf is a byte array; the words
# are little-endian. Take msg_len bytes, then the longest valid UTF-8
# prefix (mirrors the from_utf8 / valid_up_to handling in persist.rs).
print()
if 0 < msg_len <= MSG_BUF_SIZE:
    buf = bytearray()
    for w in msg_words:
        buf += w.to_bytes(4, "little")
    raw = bytes(buf[:msg_len])
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as e:
        text = raw[: e.start].decode("utf-8", "replace")
    note = "" if message_is_new else " (already displayed on LCD)"
    print(f"stored panic message ({msg_len} bytes){note}:")
    print("  " + text.replace("\n", "\n  "))
else:
    print("stored panic message: (none)")
PY
