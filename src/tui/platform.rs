//! Compile-time OS detection for platform-specific key bindings.
//!
//! Key bindings differ per platform (e.g. the default leave-terminal-focus key,
//! and macOS terminals report Command as `SUPER` for the paste shortcut).
//! Expressing that with scattered `#[cfg(...)]` attributes makes the logic hard
//! to read, so we expose one boolean constant per OS and branch on it as an
//! ordinary check (`IS_MACOS`) rather than an attribute gate.
//!
//! These are `cfg!(...)` constants folded at compile time, so branching on
//! them costs nothing at runtime and the dead arm is optimized away.

/// Whether the target OS is Windows.
pub const IS_WINDOWS: bool = cfg!(target_os = "windows");

/// Whether the target OS is Linux.
pub const IS_LINUX: bool = cfg!(target_os = "linux");

/// Whether the target OS is macOS.
pub const IS_MACOS: bool = cfg!(target_os = "macos");

/// Whether the default leave-terminal-focus binding is `Shift+Esc` rather than
/// `Alt+Esc`. Windows and Linux reserve `Alt+Esc` for cycling windows.
pub const LEAVE_FOCUS_USES_SHIFT: bool = IS_WINDOWS || IS_LINUX;

/// Whether Alt+V pastes as well as Ctrl+V. Windows Terminal's starter settings
/// bind Ctrl+V to its own paste, which never forwards the key and pastes
/// nothing for an image; Alt+V still reaches FlightDeck (and is the chord
/// Claude Code uses for image paste on Windows for the same reason).
pub const ALT_V_PASTES: bool = IS_WINDOWS;

/// User-facing label for the configured leave-terminal-focus binding.
///
/// Owned by the keymap table ([`crate::app::keymap::leave_focus_label`]).
pub fn leave_focus_key(use_f2: bool) -> &'static str {
    crate::app::keymap::leave_focus_label(crate::app::keymap::KeymapOptions::for_this_platform(
        use_f2,
    ))
}
