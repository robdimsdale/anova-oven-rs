//! Selects the active display backend from the `ui-*` feature and enforces
//! that exactly one is enabled.
//!
//! Both backends expose the same inherent async contract the display task
//! depends on — `async fn configure(&mut self)` and
//! `async fn render(&mut self, view: &ViewSpec)` — so `display.rs` and
//! `main.rs` can name `ActiveScreen` without caring which panel is wired.
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

// Exactly-one-of. Zero or two enabled features fail here with a direct
// message. Scales to N features by adding one `+ cfg!(...)` term.
const _: () = {
    let selected = cfg!(feature = "ui-lcd") as usize + cfg!(feature = "ui-sharp-basic") as usize;
    assert!(
        selected == 1,
        "enable exactly one ui-* display feature (e.g. ui-lcd or ui-sharp-basic)"
    );
};
