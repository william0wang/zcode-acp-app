//! Layout lint: guards the known layout traps of this gpui fork.
//!
//! This fork's `style_helpers!` maps EVERY fractional suffix to a RELATIVE
//! length: `size_3_5()` renders at 60% of the parent, not 14px, and
//! `py_1_5()` is 20% of the parent's WIDTH as vertical padding (taffy
//! resolves padding percentages against width), not 6px. Absolute sizing
//! must go through kit `Sizable` (`.small()`) or explicit `px(..)`.
//! Scan-only: no app code is linked, so this test compiles and runs fast.

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn no_fractional_style_helpers() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let text = fs::read_to_string(&path).expect("read source file");
                for (i, raw) in text.lines().enumerate() {
                    // Comments may quote the trapped helper while explaining it.
                    let line = raw.split("//").next().unwrap_or("");
                    if has_fractional_style_call(line) {
                        offenders.push(format!("{}:{}: {}", path.display(), i + 1, raw.trim()));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "fractional style helpers (`*_1_5()` etc.) are RELATIVE in this gpui fork — \
         size_3_5() is 60% of parent, py_1_5() is 20% of the parent WIDTH as \
         vertical padding. Use Sizable/small() or px()/rems() instead:\n{}",
        offenders.join("\n")
    );
}

/// Any `.<prefix>_<int>_<frac>(` style-helper call — the fork maps the
/// fractional suffix to `relative(..)`, which is only ever wanted for
/// widths/heights by explicit design (none exist today; add a per-prefix
/// allow-list here the day one is genuinely needed).
fn has_fractional_style_call(line: &str) -> bool {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        // A call starts at a '.' followed by a lowercase style prefix.
        if b != b'.' || i + 1 >= bytes.len() || !bytes[i + 1].is_ascii_lowercase() {
            continue;
        }
        // Scan the identifier after the dot.
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        let ident = &line[i + 1..j];
        // A fractional suffix is `_<int>_<frac>` at the identifier's tail,
        // and the identifier must end right before the call's `(`.
        let mut k = j;
        while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
            k += 1;
        }
        if k >= bytes.len() || bytes[k] != b'(' {
            continue;
        }
        if let Some(frac) = fractional_suffix(ident) {
            let _ = frac;
            return true;
        }
    }
    false
}

/// `Some((whole, frac))` when the identifier ends in `_<int>_<frac>`.
fn fractional_suffix(ident: &str) -> Option<(&str, &str)> {
    let mut parts = ident.rsplit('_');
    let frac = parts.next()?;
    if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole = parts.next()?;
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // The rest before `_<whole>_<frac>` must be non-empty (a style prefix
    // like `py`, `size`, `gap_x`), not a bare number such as `size_3_5`'s
    // own prefix check — i.e. reject `3_5(` alone but keep `py_1_5(`.
    let prefix = parts.next();
    match prefix {
        Some(p) if !p.is_empty() => Some((whole, frac)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{fractional_suffix, has_fractional_style_call};

    #[test]
    fn fractional_suffix_shapes() {
        assert_eq!(fractional_suffix("py_1_5"), Some(("1", "5")));
        assert_eq!(fractional_suffix("size_3_5"), Some(("3", "5")));
        assert_eq!(fractional_suffix("gap_x_2_5"), Some(("2", "5")));
        assert_eq!(fractional_suffix("py_2"), None);
        assert_eq!(fractional_suffix("p_4"), None);
        assert_eq!(fractional_suffix("3_5"), None);
        assert_eq!(fractional_suffix("py_1_5x"), None);
    }

    #[test]
    fn detection_spots_calls_not_words() {
        assert!(has_fractional_style_call("div().py_1_5().flex()"));
        assert!(has_fractional_style_call("x.size_3_5 ()"));
        assert!(!has_fractional_style_call("div().py_2().flex()"));
        assert!(!has_fractional_style_call("let py_1_5 = 6;"));
        assert!(!has_fractional_style_call("// explain py_1_5 here"));
    }
}
