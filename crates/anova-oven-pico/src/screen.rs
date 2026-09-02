//! Selects the active display backend from the `ui-*` feature and enforces
//! that at most one is enabled. With none enabled the firmware runs headless
//! (defmt logging only) via [`crate::display::NullScreen`].
//!
//! Every backend implements [`crate::display::DisplayBackend`]
//! (`async fn configure` / `async fn render`), so `display.rs` and `main.rs`
//! can name `ActiveScreen` without caring which panel — if any — is wired.
//!
//! Adding a layout for an existing panel: add a `ui-<panel>-<name>` feature
//! (Cargo.toml), a render module, one `cfg` alias arm below, and one
//! `+ cfg!(...)` term in the guard. A graphical panel's feature should also
//! pull in `_graphics`, which is how the rest of the crate asks "is some
//! pixel-addressed panel selected?" without listing them all.

#[cfg(feature = "ui-lcd")]
pub type ActiveScreen = crate::lcd::LcdController;

// Each arm excludes the features above it so enabling two doesn't *also* trip a
// redundant "ActiveScreen defined twice" error — the const-assert below is the
// one clear signal in that case.
#[cfg(all(feature = "ui-sharp-basic", not(feature = "ui-lcd")))]
pub type ActiveScreen = crate::sharp_ui::SharpScreen;

#[cfg(all(
    feature = "ui-oled-basic",
    not(any(feature = "ui-lcd", feature = "ui-sharp-basic"))
))]
pub type ActiveScreen = crate::oled_ui::OledScreen;

// Headless: no panel wired, `display_task` drives a no-op backend.
#[cfg(not(any(feature = "ui-lcd", feature = "_graphics")))]
pub type ActiveScreen = crate::display::NullScreen;

// At-most-one-of. Two or more enabled features fail here with a direct
// message. Scales to N features by adding one `+ cfg!(...)` term.
const _: () = {
    let selected = cfg!(feature = "ui-lcd") as usize
        + cfg!(feature = "ui-sharp-basic") as usize
        + cfg!(feature = "ui-oled-basic") as usize;
    assert!(
        selected <= 1,
        "enable at most one ui-* display feature (e.g. ui-lcd, ui-sharp-basic or ui-oled-basic); none = headless"
    );
};

// `oled-ssd1309` only picks between the OLED driver's two init tables, so on
// any other build it is a silently ineffective flag. Fail instead.
const _: () = {
    assert!(
        !cfg!(feature = "oled-ssd1309") || cfg!(feature = "ui-oled-basic"),
        "oled-ssd1309 selects the OLED controller variant; it needs ui-oled-basic"
    );
};
