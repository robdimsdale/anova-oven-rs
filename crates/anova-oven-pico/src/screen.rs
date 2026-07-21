//! Selects the active display backend from the `ui-*` feature and enforces
//! that at most one is enabled. With none enabled the firmware runs headless
//! (defmt logging only) via [`crate::display::NullScreen`].
//!
//! Every backend implements [`crate::display::DisplayBackend`]
//! (`async fn configure` / `async fn render`), so `display.rs` and `main.rs`
//! can name `ActiveScreen` without caring which panel — if any — is wired.
//!
//! Adding a Sharp layout: add a `ui-sharp-<name>` feature (Cargo.toml), a
//! render module, one `cfg` alias arm below, and one `+ cfg!(...)` term in the
//! guard.

#[cfg(feature = "ui-lcd")]
pub type ActiveScreen = crate::lcd::LcdController;

// `not(ui-lcd)` so enabling two features doesn't *also* trip a redundant
// "ActiveScreen defined twice" error — the const-assert below is the one
// clear signal in that case.
#[cfg(all(feature = "ui-sharp-basic", not(feature = "ui-lcd")))]
pub type ActiveScreen = crate::sharp_ui::SharpScreen;

// Headless: no panel wired, `display_task` drives a no-op backend.
#[cfg(not(any(feature = "ui-lcd", feature = "ui-sharp-basic")))]
pub type ActiveScreen = crate::display::NullScreen;

// At-most-one-of. Two or more enabled features fail here with a direct
// message. Scales to N features by adding one `+ cfg!(...)` term.
const _: () = {
    let selected = cfg!(feature = "ui-lcd") as usize + cfg!(feature = "ui-sharp-basic") as usize;
    assert!(
        selected <= 1,
        "enable at most one ui-* display feature (e.g. ui-lcd or ui-sharp-basic); none = headless"
    );
};
