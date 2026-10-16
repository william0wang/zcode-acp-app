//! Layout lint: guards the known layout traps of this gpui fork.
//!
//! This fork's `style_helpers!` maps fractional suffixes to RELATIVE lengths:
//! `size_3_5()` renders at 60% of the parent, not 14px. Absolute sizing must
//! go through kit `Sizable` (`.small()`) or explicit `px(..)`. Scan-only: no
//! app code is linked, so this test compiles and runs fast.

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn no_fractional_size_helpers() {
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
                    if has_fractional_size_call(line) {
                        offenders.push(format!("{}:{}: {}", path.display(), i + 1, raw.trim()));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "fractional size_*_n() helpers are RELATIVE in this gpui fork \
         (size_3_5() = 60% of parent, not 14px) — use Sizable/small() or px() \
         instead:\n{}",
        offenders.join("\n")
    );
}

fn has_fractional_size_call(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut search_from = 0;
    while let Some(pos) = line[search_from..].find("size_") {
        let mut j = search_from + pos + "size_".len();
        let int_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > int_start && j < bytes.len() && bytes[j] == b'_' {
            j += 1;
            let frac_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > frac_start {
                let mut k = j;
                while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b'(' {
                    return true;
                }
            }
        }
        search_from = search_from + pos + "size_".len();
    }
    false
}
