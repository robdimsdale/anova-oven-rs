#!/usr/bin/env bash
#
# Dump and decode the firmware's persistent crash-recording region.
#
# The anova-oven-pico firmware keeps a `PersistRegion` struct in a
# reserved, NOLOAD SRAM section (`.uninit.PERSIST`) that survives every
# reset except power-on. See crates/anova-oven-pico/src/persist.rs for
# the authoritative layout.
#
# Rather than hardcoding the layout (which silently rotted once already:
# a 2-word RingEntry / 11-word header decode kept "working" against the
# 6-word / 14-word `v3` MAGIC because the magic check only looks at
# word 0), this script *parses the layout out of persist.rs*:
#
#   * MAGIC / RING_SIZE / MSG_BUF_SIZE / RING_ENTRY_WORDS  -> `const`s
#   * header field order/count                              -> the
#     `struct PersistRegion` field list, up to and including `ring_head`
#   * ring entry field order                                -> the
#     `struct RingEntry` field list
#
# So adding or reordering a u32 field in either struct is picked up
# automatically; only a non-u32 header field or a structural change
# (e.g. ring no longer `[RingEntry; RING_SIZE]`) would need edits here.
#
# It locates the static's address with `arm-none-eabi-nm`, reads the
# region over SWD with `probe-rs`, then decodes it the same way
# `init_at_boot()` / `ring_read()` do.
#
# Usage:
#   scripts/dump-persist.sh [path/to/elf]
#
# Environment overrides:
#   CHIP      target chip for probe-rs        (default: RP2040)
#   PROTOCOL  probe-rs wire protocol          (default: swd)
#   PROFILE   cargo profile to look in        (default: release)
#   BIN       explicit ELF path               (overrides auto-detection)
#
set -euo pipefail

CHIP="${CHIP:-RP2040}"
# RP2040 needs SWD; the project's cargo runner already passes
# `--protocol swd`. Without it `probe-rs read` produces no output and
# the decode silently sees "0 words".
PROTOCOL="${PROTOCOL:-swd}"
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

PERSIST_RS="$CRATE_DIR/src/persist.rs"
PERSIST_DATA_RS="$REPO_ROOT/crates/anova-oven-pico-core/src/persist_data.rs"
for f in "$PERSIST_RS" "$PERSIST_DATA_RS"; do
  if [[ ! -f "$f" ]]; then
    echo "error: cannot find $f to read layout constants" >&2
    exit 1
  fi
done

# Name-table sources in pico-core. Parsed by the embedded python below
# (`RESET_REASON` from reset.rs, `APP_STATE` from fsm.rs + reset.rs).
# These used to be hand-maintained Python dicts in this script — that
# duplicated the Rust source and silently rotted on new variants. Now
# the script is the *only* surface that has to know how to parse them;
# adding a variant in Rust automatically updates this output.
RESET_RS="$REPO_ROOT/crates/anova-oven-pico-core/src/reset.rs"
FSM_RS="$REPO_ROOT/crates/anova-oven-pico-core/src/fsm.rs"
for f in "$RESET_RS" "$FSM_RS"; do
  if [[ ! -f "$f" ]]; then
    echo "error: cannot find $f to read enum name tables" >&2
    exit 1
  fi
done

# --- Layout, parsed from persist.rs so it can't silently drift -------------

rs_const() { # name -> integer literal (underscores stripped)
    # `const NAME` (persist.rs locals like MAGIC, RING_ENTRY_WORDS) or
    # `pub const NAME` (canonical sizing constants in pico-core,
    # re-exported via `pub use`). Try persist.rs first so a local
    # override wins; fall back to persist_data.rs for the re-exports.
    local name="$1"
    local found
    for src in "$PERSIST_RS" "$PERSIST_DATA_RS"; do
        found="$(sed -nE "s/^[[:space:]]*(pub )?const $name:[^=]*=[[:space:]]*([0-9A-Fa-fx_]+)[[:space:]]*;.*/\2/p" \
                 "$src" | head -n1 | tr -d '_')"
        if [[ -n "$found" ]]; then
            echo "$found"
            return
        fi
    done
}

# Extract the ordered `name: u32,` field names from the body of
# `struct <name> { ... }`. Stops at the first `[...]` array field (so
# for PersistRegion it ends at the `ring` array and never reaches the
# trailing `msg_len` / `msg_buf`) or the struct's closing brace. Plain
# POSIX awk only (macOS ships BSD awk — no gawk match() capture array).
struct_u32_fields() { # struct-name -> newline-separated field names
    awk -v s="$1" '
        $0 ~ ("^struct " s " \\{")     { inside = 1; next }
        inside && /^\}/                { exit }
        inside && /:[[:space:]]*\[/    { exit }   # first array field
        inside && /^[[:space:]]+[A-Za-z_][A-Za-z0-9_]*:[[:space:]]*u32,/ {
            name = $1; sub(/:.*/, "", name); print name
        }
    ' "$PERSIST_RS"
}

MAGIC_RS="$(rs_const MAGIC)"
RING_SIZE_RS="$(rs_const RING_SIZE)"
MSG_BUF_SIZE_RS="$(rs_const MSG_BUF_SIZE)"
RING_ENTRY_WORDS_RS="$(rs_const RING_ENTRY_WORDS)"
VERSION_BUF_SIZE_RS="$(rs_const VERSION_BUF_SIZE)"

if [[ -z "$MAGIC_RS" || -z "$RING_SIZE_RS" || -z "$MSG_BUF_SIZE_RS" \
      || -z "$RING_ENTRY_WORDS_RS" || -z "$VERSION_BUF_SIZE_RS" ]]; then
  echo "error: failed to parse" \
       "MAGIC/RING_SIZE/MSG_BUF_SIZE/RING_ENTRY_WORDS/VERSION_BUF_SIZE" \
       "from persist.rs / persist_data.rs" >&2
  exit 1
fi

# Header = every u32 field of PersistRegion before the `ring` array
# (magic .. ring_head, inclusive). RingEntry = its u32 fields.
HEADER_FIELDS="$(struct_u32_fields PersistRegion)"
RING_FIELDS="$(struct_u32_fields RingEntry)"
HEADER_WORDS="$(printf '%s\n' "$HEADER_FIELDS" | grep -c .)"
RING_FIELD_COUNT="$(printf '%s\n' "$RING_FIELDS" | grep -c .)"

if [[ -z "$HEADER_FIELDS" || -z "$RING_FIELDS" ]]; then
  echo "error: failed to parse struct field layout from $PERSIST_RS" >&2
  exit 1
fi
if [[ "$RING_FIELD_COUNT" -ne "$RING_ENTRY_WORDS_RS" ]]; then
  echo "error: RingEntry has $RING_FIELD_COUNT u32 fields but" \
       "RING_ENTRY_WORDS=$RING_ENTRY_WORDS_RS in $PERSIST_RS — layout" \
       "parse is out of sync; refusing to decode against bad offsets" >&2
  exit 1
fi
case "$HEADER_FIELDS" in
  *ring_head*) : ;;
  *) echo "error: no 'ring_head' field found among PersistRegion u32" \
          "fields — struct layout changed in an unexpected way" >&2
     exit 1 ;;
esac

# Region size in u32 words, fully derived from the parsed layout:
#   HEADER_WORDS                       (magic .. ring_head)
# + RING_SIZE * RING_ENTRY_WORDS       (the ring)
# + 1                                  (msg_len)
# + MSG_BUF_SIZE / 4                   (msg_buf bytes)
# + VERSION_BUF_SIZE / 4               (version_buf bytes)
WORDS=$(( HEADER_WORDS + RING_SIZE_RS * RING_ENTRY_WORDS_RS \
          + 1 + MSG_BUF_SIZE_RS / 4 + VERSION_BUF_SIZE_RS / 4 ))

echo "ELF:     $ELF"
echo "symbol:  0x$ADDR (.uninit.PERSIST)"
echo "chip:    $CHIP   protocol: $PROTOCOL"
echo "layout:  MAGIC=$MAGIC_RS RING_SIZE=$RING_SIZE_RS" \
     "RING_ENTRY_WORDS=$RING_ENTRY_WORDS_RS" \
     "MSG_BUF_SIZE=$MSG_BUF_SIZE_RS" \
     "VERSION_BUF_SIZE=$VERSION_BUF_SIZE_RS"
echo "         header=$HEADER_WORDS words -> $WORDS words total" \
     "(from persist.rs)"
echo

# `probe-rs read` writes the words to stdout. Capture stderr separately
# so a probe/attach failure surfaces instead of being swallowed into an
# empty (and silently mis-decoded) read.
PR_ERR="$(mktemp)"
trap 'rm -f "$PR_ERR"' EXIT
if ! RAW="$(probe-rs read --chip "$CHIP" --protocol "$PROTOCOL" \
            b32 "0x$ADDR" "$WORDS" 2>"$PR_ERR")"; then
  echo "error: 'probe-rs read' failed:" >&2
  cat "$PR_ERR" >&2
  exit 1
fi
GOT="$(echo "$RAW" | wc -w | tr -d ' ')"
if [[ "$GOT" -lt "$WORDS" ]]; then
  echo "error: expected $WORDS words from probe-rs, got $GOT" >&2
  [[ -s "$PR_ERR" ]] && { echo "probe-rs stderr:" >&2; cat "$PR_ERR" >&2; }
  exit 1
fi

# Hand the flat word list + parsed layout to python for the decode. The
# decode *logic* mirrors init_at_boot()/ring_read(); all offsets come
# from the layout parsed above, passed through the environment.
#
# NOTE: the word list goes through the environment, NOT a stdin pipe.
# `python3 - <<'PY'` reads the *program* from stdin (that's what `-`
# means), so a `echo "$RAW" | python3 - <<'PY'` pipe is shadowed by the
# heredoc and the script sees empty stdin. (That latent bug is why the
# original version of this script reported "got 0" for every read.)
export MAGIC_RS RING_SIZE_RS MSG_BUF_SIZE_RS RING_ENTRY_WORDS_RS \
       VERSION_BUF_SIZE_RS HEADER_FIELDS RING_FIELDS RESET_RS FSM_RS
export PERSIST_WORDS="$RAW"
python3 - "$WORDS" <<'PY'
import os
import re
import sys

expected = int(sys.argv[1])
words = [int(tok, 16) for tok in os.environ["PERSIST_WORDS"].split()]

if len(words) < expected:
    sys.exit(f"error: expected {expected} words, got {len(words)}")

MAGIC = int(os.environ["MAGIC_RS"], 0)
RING_SIZE = int(os.environ["RING_SIZE_RS"], 0)
MSG_BUF_SIZE = int(os.environ["MSG_BUF_SIZE_RS"], 0)
RING_ENTRY_WORDS = int(os.environ["RING_ENTRY_WORDS_RS"], 0)
VERSION_BUF_SIZE = int(os.environ["VERSION_BUF_SIZE_RS"], 0)
HEADER = [f for f in os.environ["HEADER_FIELDS"].split() if f]
RING_FIELDS = [f for f in os.environ["RING_FIELDS"].split() if f]


def read(path):
    with open(path, "r", encoding="utf-8") as fp:
        return fp.read()


RESET_SRC = read(os.environ["RESET_RS"])
FSM_SRC = read(os.environ["FSM_RS"])


def die(msg):
    sys.exit(
        f"error: {msg}\n"
        "       enum-name parse out of sync with Rust source — refusing to\n"
        "       decode against possibly-stale labels (would mis-attribute\n"
        "       resets in the output). Update this script's regexes or fix\n"
        "       the Rust source to match."
    )


# --- RESET_REASON: `#[repr(u32)] enum ResetReason { Variant = N, ... }`
#                   + `fn name(&self) -> &'static str { Self::Variant => "Label", ... }`
#
# The first pass extracts `Variant -> u32`; the second pass extracts
# `Variant -> label`; we compose to `u32 -> label`. Going through the
# variant name (rather than assuming positional order) means an
# explicit discriminant skip (`= 7`) wouldn't silently misalign.

def parse_reset_reason(src):
    enum_body = re.search(
        r"pub enum ResetReason\s*\{(.*?)\}",
        src,
        re.DOTALL,
    )
    if not enum_body:
        die("could not locate `pub enum ResetReason { ... }` in reset.rs")
    variants = dict(
        re.findall(
            r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(\d+)\s*,",
            enum_body.group(1),
            re.MULTILINE,
        )
    )
    if not variants:
        die("found ResetReason but no `Variant = N,` arms — enum body shape changed")

    name_fn = re.search(
        r"pub fn name\(&self\)\s*->\s*&'static str\s*\{\s*match self\s*\{(.*?)\}\s*\}",
        src,
        re.DOTALL,
    )
    if not name_fn:
        die("could not locate `ResetReason::name(&self)` match in reset.rs")
    labels = dict(
        re.findall(
            r"Self::([A-Za-z_][A-Za-z0-9_]*)\s*=>\s*\"([^\"]+)\"",
            name_fn.group(1),
        )
    )
    if not labels:
        die("found ResetReason::name but no `Self::Variant => \"Label\",` arms")

    missing = set(variants) - set(labels)
    if missing:
        die(f"ResetReason variants without a name() arm: {sorted(missing)}")

    return {int(variants[v]): labels[v] for v in variants}


# --- APP_STATE: union of `fn app_state_name(d: u32)` arms in fsm.rs
#                and `fn init_stage_name(d: u32)` arms in reset.rs.
#
# Both have the shape `N => Some("Label"),` (app_state_name) or
# `CONST_NAME => Some("..."),` (init_stage_name). The init_stage_name
# arms key off the `pub const INIT_STAGE_*: u32 = N;` declarations, so
# we also have to resolve those — same parse-from-source pattern.

def parse_const_u32_table(src):
    """`pub const NAME: u32 = N;` -> {NAME: N}."""
    return dict(
        re.findall(
            r"^\s*pub const ([A-Z][A-Z0-9_]*):\s*u32\s*=\s*(\d+)\s*;",
            src,
            re.MULTILINE,
        )
    )


def parse_some_label_table(src, fn_name, const_table):
    """Extract `key => Some("label"),` arms from a single match-fn body.

    Keys are either numeric literals or const identifiers; const names
    are resolved against `const_table`.
    """
    body = re.search(
        rf"fn {fn_name}\s*\([^)]*\)\s*->\s*Option<&'static str>\s*\{{\s*"
        r"match\s+d\s*\{(.*?)\}\s*\}",
        src,
        re.DOTALL,
    )
    if not body:
        die(f"could not locate `fn {fn_name}` match-on-`d` in source")
    table = {}
    for key, label in re.findall(
        r'^\s*([A-Za-z0-9_]+)\s*=>\s*Some\("([^"]+)"\)\s*,',
        body.group(1),
        re.MULTILINE,
    ):
        if key.isdigit():
            num = int(key)
        elif key in const_table:
            num = int(const_table[key])
        else:
            # `_ => None,` is the wildcard arm and isn't returned here
            # because it doesn't match `Some("...")`. Any other unknown
            # identifier is a real parse failure.
            die(f"`{fn_name}` arm key `{key}` is not a number nor a known const")
        table[num] = label
    if not table:
        die(f"`{fn_name}` parsed to an empty table — arm shape changed?")
    return table


reset_consts = parse_const_u32_table(RESET_SRC)
APP_STATE = {}
APP_STATE.update(parse_some_label_table(FSM_SRC, "app_state_name", {}))
APP_STATE.update(parse_some_label_table(RESET_SRC, "init_stage_name", reset_consts))
RESET_REASON = parse_reset_reason(RESET_SRC)

# Sanity: persist.rs stores ResetReason::ColdBoot as 1 in
# init_at_boot (and 0 = Unknown for unset/raw values). If those are
# missing the layout/values have drifted in a way we won't notice
# until a real crash dump.
for required in (0, 1):
    if required not in RESET_REASON:
        die(f"RESET_REASON parsed without entry for {required} — "
            f"enum discriminants changed?")
# Same idea for AppState: 1..=8 are persisted across resets and the
# INIT_STAGE_* sentinels are the bring-up breadcrumbs.
for required in (1, 2, 3, 4, 5, 6, 7, 8, 100, 101):
    if required not in APP_STATE:
        die(f"APP_STATE parsed without entry for {required} — "
            f"discriminant() or INIT_STAGE_* changed?")


def reason(v):
    return RESET_REASON.get(v, f"Unknown({v})")


def app_state(v):
    return f"{v} = {APP_STATE[v]}" if v in APP_STATE else f"{v} = Unknown({v})"


def hms(secs):
    h, rem = divmod(secs, 3600)
    m, s = divmod(rem, 60)
    if h:
        return f"{secs}s (~{h}h {m}m {s}s)"
    if m:
        return f"{secs}s (~{m}m {s}s)"
    return f"{secs}s"


def kib(b):
    return f"{b} bytes (~{b / 1024:.1f} KiB)"


def updown(v):
    return f"{v} ({'up' if v else 'down'})"


# Build version: lives in version_buf at the very end of the region
# (immediately after msg_buf). Print it first so an operator who's
# debugging an unknown device sees "which image is this?" before
# anything else. The slot is NUL-padded; truncate at the first NUL and
# best-effort UTF-8 decode (same convention as msg_buf below).
version_start = (
    len(HEADER) + RING_SIZE * RING_ENTRY_WORDS + 1 + MSG_BUF_SIZE // 4
)
version_words = words[version_start:version_start + VERSION_BUF_SIZE // 4]
version_bytes = bytearray()
for w in version_words:
    version_bytes += w.to_bytes(4, "little")
nul = version_bytes.find(0)
if nul >= 0:
    version_bytes = version_bytes[:nul]
try:
    version_str = version_bytes.decode("utf-8")
except UnicodeDecodeError as e:
    version_str = version_bytes[: e.start].decode("utf-8", "replace")
print(f"build version: {version_str or '(empty)'}")
print()

# Header: one u32 per parsed field name, in struct order.
hdr = dict(zip(HEADER, words))
magic = hdr["magic"]

if magic != MAGIC:
    print(f"magic:   0x{magic:08x}  ** MISMATCH **  "
          f"(expected 0x{MAGIC:08x})")
    print()
    print("Region is invalid: a cold boot since power-on, RAM was lost,")
    print("or the ELF doesn't match the flashed firmware (wrong PERSIST")
    print("address). Decoded fields below are NOT trustworthy.")
    print()

# Pretty-print every header field; special-case the ones with units.
for name in HEADER:
    v = hdr[name]
    if name == "magic":
        tag = "  (valid)" if v == MAGIC else "  ** MISMATCH **"
        print(f"{name:<28} 0x{v:08x}{tag}")
    elif name == "reset_reason":
        print(f"{name:<28} {v} = {reason(v)}")
    elif name == "last_app_state":
        print(f"{name:<28} {app_state(v)}")
    elif name.endswith("_secs"):
        print(f"{name:<28} {hms(v)}")
    elif name == "network_up":
        print(f"{name:<28} {updown(v)}")
    elif "free_heap" in name:
        print(f"{name:<28} {kib(v)}")
    else:
        print(f"{name:<28} {v}")

panic_count = hdr.get("panic_count", 0)
last_displayed = hdr.get("last_displayed_panic_count", 0)
message_is_new = panic_count > last_displayed
print(f"{'message_is_new':<28} {message_is_new}")

# Ring: RING_SIZE entries of RING_ENTRY_WORDS each, starting right after
# the header. ring_read(): newest first,
#   idx = (head + RING_SIZE - 1 - i) % RING_SIZE, for i in 0..min(head,N)
ring_head = hdr["ring_head"]
ring_base = len(HEADER)
print()
count = min(ring_head, RING_SIZE)
if count == 0:
    print("reset history: (empty — no non-cold-boot resets recorded)")
else:
    print(f"reset history (newest first, {count} of max {RING_SIZE}):")
    for i in range(count):
        idx = (ring_head + RING_SIZE - 1 - i) % RING_SIZE
        off = ring_base + idx * RING_ENTRY_WORDS
        e = dict(zip(RING_FIELDS, words[off:off + RING_ENTRY_WORDS]))
        parts = []
        for fn in RING_FIELDS:
            v = e[fn]
            if fn == "reset_reason":
                parts.append(reason(v))
            elif fn.endswith("_secs"):
                parts.append(f"{fn}={hms(v)}")
            elif fn == "network_up":
                parts.append(f"{fn}={'up' if v else 'down'}")
            elif "free_heap" in fn:
                parts.append(f"{fn}={v}B(~{v / 1024:.1f}KiB)")
            else:
                parts.append(f"{fn}={v}")
        tag = "  <- most recent" if i == 0 else ""
        print(f"  [{i}] " + "  ".join(parts) + tag)

# msg_len then msg_buf, immediately after the ring.
msg_len_idx = len(HEADER) + RING_SIZE * RING_ENTRY_WORDS
msg_len = words[msg_len_idx]
msg_words = words[msg_len_idx + 1:msg_len_idx + 1 + MSG_BUF_SIZE // 4]
print()
print(f"msg_len: {msg_len}")
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
