//! Keyboard input handling for the terminal emulator.
//!
//! This module provides [`keystroke_to_bytes`], which converts GPUI keyboard
//! events into terminal escape sequences that can be written to the PTY.
//!
//! # Key Mappings
//!
//! ## Special Keys
//!
//! | Key | Sequence | Notes |
//! |-----|----------|-------|
//! | Enter | `\r` (0x0D) | Carriage return |
//! | Escape | `\x1b` (0x1B) | ESC |
//! | Backspace | `\x7f` (0x7F) | DEL |
//! | Tab | `\t` (0x09) | Horizontal tab |
//! | Shift+Tab | `\x1b[Z` | Backtab |
//! | Space | ` ` (0x20) | Space |
//! | Ctrl+Space | `\x00` | NUL |
//!
//! ## Arrow Keys
//!
//! Arrow key sequences depend on application cursor mode:
//!
//! | Key | Normal Mode | App Cursor Mode |
//! |-----|-------------|-----------------|
//! | Up | `\x1b[A` | `\x1bOA` |
//! | Down | `\x1b[B` | `\x1bOB` |
//! | Right | `\x1b[C` | `\x1bOC` |
//! | Left | `\x1b[D` | `\x1bOD` |
//!
//! ## Navigation Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | Home | `\x1b[H` |
//! | End | `\x1b[F` |
//! | PageUp | `\x1b[5~` |
//! | PageDown | `\x1b[6~` |
//! | Insert | `\x1b[2~` |
//! | Delete | `\x1b[3~` |
//!
//! ## Modified Cursor and Navigation Keys
//!
//! Shift/Alt/Ctrl on the arrows, Home/End and the tilde family switch to
//! xterm's modifier form, `CSI 1;N A`-style, where
//! `N = 1 + shift + 2*alt + 4*ctrl`. Without it Ctrl+Left (word-wise
//! movement in readline/zsh) and the rest reach the shell as nothing at all.
//!
//! | Key | Sequence |
//! |-----|----------|
//! | Shift+Left | `\x1b[1;2D` |
//! | Alt+Left | `\x1b[1;3D` |
//! | Ctrl+Left | `\x1b[1;5D` |
//! | Ctrl+Shift+Right | `\x1b[1;6C` |
//! | Ctrl+Home | `\x1b[1;5H` |
//! | Ctrl+Delete | `\x1b[3;5~` |
//!
//! ## Function Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | F1-F4 | `\x1bOP` - `\x1bOS` |
//! | F5-F12 | `\x1b[15~` - `\x1b[24~` |
//!
//! ## Control Combinations
//!
//! Ctrl+A through Ctrl+Z map to ASCII control characters 0x01-0x1A:
//!
//! | Combination | Byte |
//! |-------------|------|
//! | Ctrl+A | 0x01 |
//! | Ctrl+C | 0x03 (interrupt) |
//! | Ctrl+D | 0x04 (EOF) |
//! | Ctrl+Z | 0x1A (suspend) |
//!
//! ## Alt Combinations
//!
//! Alt+key sends ESC followed by the key: `\x1b` + key
//!
//! # Terminal Mode Effects
//!
//! The [`TermMode`] flags affect key sequences:
//!
//! - **APP_CURSOR**: Changes arrow key sequences from CSI to SS3 format
//!
//! # Example
//!
//! ```
//! use gpui::Keystroke;
//! use alacritty_terminal::term::TermMode;
//! use gpui_terminal::input::keystroke_to_bytes;
//!
//! // Enter key
//! let keystroke = Keystroke::parse("enter").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(b"\r".to_vec()));
//!
//! // Ctrl+C (interrupt)
//! let keystroke = Keystroke::parse("ctrl-c").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(vec![0x03]));
//! ```

use alacritty_terminal::term::TermMode;
use gpui::{Keystroke, Modifiers};

/// xterm's modifier parameter for the `CSI 1;N` form:
/// `1 + shift + 2*alt + 4*ctrl + 8*platform`. `None` when no modifier is
/// held, so the caller can emit the plain (unparameterised) sequence.
fn csi_modifier(modifiers: Modifiers) -> Option<u8> {
    let n = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.control)
        + 8 * u8::from(modifiers.platform);
    (n > 1).then_some(n)
}

/// `CSI <final>` or `CSI 1;<n><final>` — arrows and Home/End.
fn csi_cursor(final_byte: u8, modifiers: Modifiers) -> Vec<u8> {
    match csi_modifier(modifiers) {
        Some(n) => format!("\x1b[1;{n}{}", final_byte as char).into_bytes(),
        None => vec![b'\x1b', b'[', final_byte],
    }
}

/// `CSI <code>~` or `CSI <code>;<n>~` — the tilde family (Ins/Del/PgUp/PgDn,
/// F5-F12).
fn csi_tilde(code: u8, modifiers: Modifiers) -> Vec<u8> {
    match csi_modifier(modifiers) {
        Some(n) => format!("\x1b[{code};{n}~").into_bytes(),
        None => format!("\x1b[{code}~").into_bytes(),
    }
}

/// `SS3 <final>` (F1-F4) or `CSI 1;<n><final>` when modified.
fn ss3_or_csi(final_byte: u8, modifiers: Modifiers) -> Vec<u8> {
    match csi_modifier(modifiers) {
        Some(n) => format!("\x1b[1;{n}{}", final_byte as char).into_bytes(),
        None => vec![b'\x1b', b'O', final_byte],
    }
}

/// Convert a GPUI keystroke to terminal escape sequence bytes.
///
/// This function translates GPUI keyboard events into the appropriate byte sequences
/// expected by terminal applications. It handles special keys, control characters,
/// and application cursor mode.
///
/// # Arguments
///
/// * `keystroke` - The GPUI keystroke to convert
/// * `mode` - The current terminal mode (affects arrow key sequences)
///
/// # Returns
///
/// An optional vector of bytes representing the terminal escape sequence.
/// Returns `None` if the keystroke should not produce any output.
///
/// # Examples
///
/// ```
/// use gpui::Keystroke;
/// use alacritty_terminal::term::TermMode;
/// use gpui_terminal::input::keystroke_to_bytes;
///
/// let keystroke = Keystroke::parse("enter").unwrap();
/// let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
/// assert_eq!(bytes, Some(b"\r".to_vec()));
/// ```
pub fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    // Handle special keys first
    match keystroke.key.as_str() {
        // Basic control characters
        "space" => {
            if keystroke.modifiers.control {
                return Some(b"\x00".to_vec()); // Ctrl+Space = NUL
            }
            return Some(b" ".to_vec());
        }
        "enter" => return Some(b"\r".to_vec()),
        "escape" => return Some(b"\x1b".to_vec()),
        "backspace" => return Some(b"\x7f".to_vec()),
        "tab" => {
            // Shift+Tab sends a different sequence
            if keystroke.modifiers.shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(b"\t".to_vec());
        }

        // Arrow keys - APP_CURSOR picks SS3 only for the UNMODIFIED form
        // (xterm's rule: a modified arrow always takes the CSI 1;N spelling).
        "up" => {
            if csi_modifier(keystroke.modifiers).is_none() && mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOA".to_vec());
            }
            return Some(csi_cursor(b'A', keystroke.modifiers));
        }
        "down" => {
            if csi_modifier(keystroke.modifiers).is_none() && mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOB".to_vec());
            }
            return Some(csi_cursor(b'B', keystroke.modifiers));
        }
        "right" => {
            if csi_modifier(keystroke.modifiers).is_none() && mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOC".to_vec());
            }
            return Some(csi_cursor(b'C', keystroke.modifiers));
        }
        "left" => {
            if csi_modifier(keystroke.modifiers).is_none() && mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOD".to_vec());
            }
            return Some(csi_cursor(b'D', keystroke.modifiers));
        }

        // Navigation keys
        "home" => return Some(csi_cursor(b'H', keystroke.modifiers)),
        "end" => return Some(csi_cursor(b'F', keystroke.modifiers)),
        "pageup" => return Some(csi_tilde(5, keystroke.modifiers)),
        "pagedown" => return Some(csi_tilde(6, keystroke.modifiers)),
        "insert" => return Some(csi_tilde(2, keystroke.modifiers)),
        "delete" => return Some(csi_tilde(3, keystroke.modifiers)),

        // Function keys
        "f1" => return Some(ss3_or_csi(b'P', keystroke.modifiers)),
        "f2" => return Some(ss3_or_csi(b'Q', keystroke.modifiers)),
        "f3" => return Some(ss3_or_csi(b'R', keystroke.modifiers)),
        "f4" => return Some(ss3_or_csi(b'S', keystroke.modifiers)),
        "f5" => return Some(csi_tilde(15, keystroke.modifiers)),
        "f6" => return Some(csi_tilde(17, keystroke.modifiers)),
        "f7" => return Some(csi_tilde(18, keystroke.modifiers)),
        "f8" => return Some(csi_tilde(19, keystroke.modifiers)),
        "f9" => return Some(csi_tilde(20, keystroke.modifiers)),
        "f10" => return Some(csi_tilde(21, keystroke.modifiers)),
        "f11" => return Some(csi_tilde(23, keystroke.modifiers)),
        "f12" => return Some(csi_tilde(24, keystroke.modifiers)),

        _ => {}
    }

    // Handle Ctrl+key combinations
    if keystroke.modifiers.control {
        let key = keystroke.key.as_str();

        // Ctrl+A through Ctrl+Z map to 0x01 through 0x1a
        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                // Convert to uppercase and then to control character
                let upper = ch.to_ascii_uppercase();
                let ctrl_char = (upper as u8) - b'@';
                return Some(vec![ctrl_char]);
            }

            // Special Ctrl combinations
            match ch {
                '[' => return Some(b"\x1b".to_vec()),  // Ctrl+[
                '\\' => return Some(b"\x1c".to_vec()), // Ctrl+\
                ']' => return Some(b"\x1d".to_vec()),  // Ctrl+]
                '^' => return Some(b"\x1e".to_vec()),  // Ctrl+^
                '_' => return Some(b"\x1f".to_vec()),  // Ctrl+_
                '?' => return Some(b"\x7f".to_vec()),  // Ctrl+?
                _ => {}
            }
        }
    }

    // Handle Alt+key combinations
    if keystroke.modifiers.alt {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            // Alt+key sends ESC followed by the key
            let ch = key.chars().next().unwrap();
            if ch.is_ascii() {
                let mut bytes = vec![b'\x1b'];
                bytes.push(ch as u8);
                return Some(bytes);
            }
        }
    }

    // Handle regular printable characters
    // Use key_char if available (contains the actual typed character with modifiers like Shift)
    if let Some(key_char) = &keystroke.key_char
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
    {
        return Some(key_char.as_bytes().to_vec());
    }

    // Fallback to key for single characters
    let key = keystroke.key.as_str();
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        // Platform (⌘) combos are the host's: without this an unhandled
        // ⌘C would type a literal "c" into the shell.
        if ch.is_ascii() && !keystroke.modifiers.control && !keystroke.modifiers.platform {
            // Handle shift modifier for uppercase
            let ch = if keystroke.modifiers.shift {
                ch.to_ascii_uppercase()
            } else {
                ch
            };
            return Some(vec![ch as u8]);
        }
        // For non-ASCII characters, encode as UTF-8
        if !keystroke.modifiers.control && !keystroke.modifiers.alt && !keystroke.modifiers.platform
        {
            return Some(key.as_bytes().to_vec());
        }
    }

    // If we get here, the keystroke doesn't produce any output
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enter_key() {
        let keystroke = Keystroke::parse("enter").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn test_escape_key() {
        let keystroke = Keystroke::parse("escape").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b".to_vec()));
    }

    #[test]
    fn test_backspace_key() {
        let keystroke = Keystroke::parse("backspace").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x7f".to_vec()));
    }

    #[test]
    fn test_tab_key() {
        let keystroke = Keystroke::parse("tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\t".to_vec()));
    }

    #[test]
    fn test_shift_tab() {
        let keystroke = Keystroke::parse("shift-tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn test_arrow_keys_normal_mode() {
        let mode = TermMode::empty();

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1b[A".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1b[B".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1b[C".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn test_arrow_keys_app_cursor_mode() {
        let mode = TermMode::APP_CURSOR;

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1bOA".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1bOB".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1bOC".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn test_navigation_keys() {
        let mode = TermMode::empty();

        let home = Keystroke::parse("home").unwrap();
        assert_eq!(keystroke_to_bytes(&home, mode), Some(b"\x1b[H".to_vec()));

        let end = Keystroke::parse("end").unwrap();
        assert_eq!(keystroke_to_bytes(&end, mode), Some(b"\x1b[F".to_vec()));

        let pageup = Keystroke::parse("pageup").unwrap();
        assert_eq!(keystroke_to_bytes(&pageup, mode), Some(b"\x1b[5~".to_vec()));

        let pagedown = Keystroke::parse("pagedown").unwrap();
        assert_eq!(
            keystroke_to_bytes(&pagedown, mode),
            Some(b"\x1b[6~".to_vec())
        );

        let insert = Keystroke::parse("insert").unwrap();
        assert_eq!(keystroke_to_bytes(&insert, mode), Some(b"\x1b[2~".to_vec()));

        let delete = Keystroke::parse("delete").unwrap();
        assert_eq!(keystroke_to_bytes(&delete, mode), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn test_function_keys() {
        let mode = TermMode::empty();

        let f1 = Keystroke::parse("f1").unwrap();
        assert_eq!(keystroke_to_bytes(&f1, mode), Some(b"\x1bOP".to_vec()));

        let f2 = Keystroke::parse("f2").unwrap();
        assert_eq!(keystroke_to_bytes(&f2, mode), Some(b"\x1bOQ".to_vec()));

        let f5 = Keystroke::parse("f5").unwrap();
        assert_eq!(keystroke_to_bytes(&f5, mode), Some(b"\x1b[15~".to_vec()));

        let f12 = Keystroke::parse("f12").unwrap();
        assert_eq!(keystroke_to_bytes(&f12, mode), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn test_ctrl_combinations() {
        let mode = TermMode::empty();

        // Ctrl+A = 0x01
        let ctrl_a = Keystroke::parse("ctrl-a").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_a, mode), Some(vec![0x01]));

        // Ctrl+C = 0x03
        let ctrl_c = Keystroke::parse("ctrl-c").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_c, mode), Some(vec![0x03]));

        // Ctrl+Z = 0x1a
        let ctrl_z = Keystroke::parse("ctrl-z").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_z, mode), Some(vec![0x1a]));

        // Ctrl+Space = 0x00
        let ctrl_space = Keystroke::parse("ctrl-space").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_space, mode), Some(vec![0x00]));
    }

    #[test]
    fn test_alt_combinations() {
        let mode = TermMode::empty();

        // Alt+a sends ESC followed by 'a'
        let alt_a = Keystroke::parse("alt-a").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_a, mode), Some(b"\x1ba".to_vec()));

        // Alt+x sends ESC followed by 'x'
        let alt_x = Keystroke::parse("alt-x").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_x, mode), Some(b"\x1bx".to_vec()));
    }

    #[test]
    fn test_regular_characters() {
        let mode = TermMode::empty();

        let a = Keystroke::parse("a").unwrap();
        assert_eq!(keystroke_to_bytes(&a, mode), Some(b"a".to_vec()));

        let z = Keystroke::parse("z").unwrap();
        assert_eq!(keystroke_to_bytes(&z, mode), Some(b"z".to_vec()));

        let zero = Keystroke::parse("0").unwrap();
        assert_eq!(keystroke_to_bytes(&zero, mode), Some(b"0".to_vec()));
    }

    #[test]
    fn test_space_key() {
        let mode = TermMode::empty();

        let space = Keystroke::parse("space").unwrap();
        assert_eq!(keystroke_to_bytes(&space, mode), Some(b" ".to_vec()));
    }

    /// Modified cursor keys take xterm's `CSI 1;N` form — Ctrl+Left is
    /// word-wise movement in readline/zsh and is unreachable without it.
    /// N = 1 + shift + 2*alt + 4*ctrl.
    #[test]
    fn test_modified_arrow_keys() {
        let mode = TermMode::empty();

        let shift_left = Keystroke::parse("shift-left").unwrap();
        assert_eq!(
            keystroke_to_bytes(&shift_left, mode),
            Some(b"\x1b[1;2D".to_vec())
        );

        let alt_right = Keystroke::parse("alt-right").unwrap();
        assert_eq!(
            keystroke_to_bytes(&alt_right, mode),
            Some(b"\x1b[1;3C".to_vec())
        );

        let ctrl_left = Keystroke::parse("ctrl-left").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_left, mode),
            Some(b"\x1b[1;5D".to_vec())
        );

        let ctrl_shift_right = Keystroke::parse("ctrl-shift-right").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_shift_right, mode),
            Some(b"\x1b[1;6C".to_vec())
        );

        let ctrl_up = Keystroke::parse("ctrl-up").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_up, mode),
            Some(b"\x1b[1;5A".to_vec())
        );

        // A modified arrow overrides APP_CURSOR: xterm only uses SS3 for the
        // bare form.
        let ctrl_left_app = Keystroke::parse("ctrl-left").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_left_app, TermMode::APP_CURSOR),
            Some(b"\x1b[1;5D".to_vec())
        );
    }

    #[test]
    fn test_modified_navigation_and_function_keys() {
        let mode = TermMode::empty();

        let ctrl_home = Keystroke::parse("ctrl-home").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_home, mode),
            Some(b"\x1b[1;5H".to_vec())
        );

        let ctrl_delete = Keystroke::parse("ctrl-delete").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_delete, mode),
            Some(b"\x1b[3;5~".to_vec())
        );

        let shift_pagedown = Keystroke::parse("shift-pagedown").unwrap();
        assert_eq!(
            keystroke_to_bytes(&shift_pagedown, mode),
            Some(b"\x1b[6;2~".to_vec())
        );

        let shift_f1 = Keystroke::parse("shift-f1").unwrap();
        assert_eq!(
            keystroke_to_bytes(&shift_f1, mode),
            Some(b"\x1b[1;2P".to_vec())
        );

        let ctrl_f5 = Keystroke::parse("ctrl-f5").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_f5, mode),
            Some(b"\x1b[15;5~".to_vec())
        );
    }

    /// Command combos belong to the host app: typing ⌘C must not insert a
    /// literal "c" into the shell.
    #[test]
    fn test_platform_modified_keys_produce_nothing() {
        let mode = TermMode::empty();

        let cmd_c = Keystroke::parse("cmd-c").unwrap();
        assert_eq!(keystroke_to_bytes(&cmd_c, mode), None);

        let cmd_q = Keystroke::parse("cmd-q").unwrap();
        assert_eq!(keystroke_to_bytes(&cmd_q, mode), None);
    }
}
