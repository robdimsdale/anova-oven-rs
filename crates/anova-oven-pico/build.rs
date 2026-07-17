use std::process::Command;

fn main() {
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    // BUILD_VERSION is consumed by main.rs (boot banner) and persist.rs
    // (recorded into the persist MMIO version slot so it surfaces on
    // /health and via dump-persist over SWD). Falls back to "unknown" SHA
    // when building outside a git checkout (CI tarball, vendored build).
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap();
    let sha = git_short_sha().unwrap_or_else(|| "unknown".into());
    let dirty = if git_is_dirty() { "-dirty" } else { "" };
    println!("cargo:rustc-env=BUILD_VERSION={pkg}-{sha}{dirty}");

    // Without these, an existing dev binary keeps reporting a stale SHA
    // until the next `cargo clean`. The paths are relative to this
    // crate's manifest dir; the repo root is two levels up.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    // memory.x is the linker's layout contract (the linker finds it via the
    // link cwd) and defines the `__bootloader_*` partition symbols that
    // `ota.rs` reads at runtime via `addr_of!`. This script no longer parses
    // it, but a layout edit must still force a re-link, hence the rerun hook.
    println!("cargo:rerun-if-changed=memory.x");
}

fn git_short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.into())
    }
}

fn git_is_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|o| !o.stdout.is_empty())
}

