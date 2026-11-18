//! Unified line-diff rows for tool-card diff blocks — the desktop half of
//! TS ChatView's `DiffBlock` (ReactDiffViewer). ACP edit results carry
//! whole-file old/new text pairs (`AcpDiffContent`), not unified-diff
//! strings, so the rows are computed here with a plain LCS. Blocks are
//! built at the snapshot sync point (once per content change), never per
//! frame; `unified` is pure and unit-pinned.

use protocol::acp::AcpDiffContent;

/// Per-side character cap (TS ChatView's `MAX_DIFF_CHARS`): jsdiff on
/// whole-file texts can wedge a WebView for seconds — the same bound keeps
/// the LCS cheap here, and the card says so via `chat.diffTruncated`.
const MAX_DIFF_CHARS: usize = 20_000;

/// LCS DP cell budget: past this the middle section falls back to
/// delete-all + add-all rows (still a faithful diff, just not minimal) so
/// a pathological pair can never allocate unbounded memory.
const MAX_DP_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    /// 1-based source line numbers (req 2.1): the old side's row for
    /// Context/Del, the new side's for Context/Add — None where the row
    /// has no presence on that side.
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffBlock {
    pub path: String,
    pub lines: Vec<DiffLine>,
    pub truncated: bool,
}

fn line(kind: DiffLineKind, text: &str) -> DiffLine {
    DiffLine {
        kind,
        text: text.to_string(),
        old_no: None,
        new_no: None,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// 1-based source numbering accumulator (req 2.1): old advances on
/// Context/Del, new on Context/Add.
#[derive(Default)]
struct Numbering {
    old_no: u32,
    new_no: u32,
}

impl Numbering {
    fn stamp(&mut self, kind: DiffLineKind, text: &str) -> DiffLine {
        let mut row = line(kind, text);
        match kind {
            DiffLineKind::Context => {
                self.old_no += 1;
                self.new_no += 1;
                row.old_no = Some(self.old_no);
                row.new_no = Some(self.new_no);
            }
            DiffLineKind::Del => {
                self.old_no += 1;
                row.old_no = Some(self.old_no);
            }
            DiffLineKind::Add => {
                self.new_no += 1;
                row.new_no = Some(self.new_no);
            }
        }
        row
    }
}

/// Whole-file old/new texts → unified rows. Common prefix/suffix are
/// trimmed first (the common case is a small edit inside a large file), so
/// the DP only covers the changed middle. Returns the rows and whether a
/// side was capped (`truncated`).
pub fn unified(old: &str, new: &str) -> (Vec<DiffLine>, bool) {
    let old_capped = old.chars().count() > MAX_DIFF_CHARS;
    let new_capped = new.chars().count() > MAX_DIFF_CHARS;
    let old = truncate_chars(old, MAX_DIFF_CHARS);
    let new = truncate_chars(new, MAX_DIFF_CHARS);
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    let mut lines = Vec::new();
    let mut num = Numbering::default();
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let (mut end_a, mut end_b) = (a.len(), b.len());
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    for l in &a[..start] {
        lines.push(num.stamp(DiffLineKind::Context, l));
    }

    let mid_a = &a[start..end_a];
    let mid_b = &b[start..end_b];
    let (n, m) = (mid_a.len(), mid_b.len());
    if (n + 1) * (m + 1) > MAX_DP_CELLS {
        for l in mid_a {
            lines.push(num.stamp(DiffLineKind::Del, l));
        }
        for l in mid_b {
            lines.push(num.stamp(DiffLineKind::Add, l));
        }
    } else {
        // dp[i][j] = LCS length of mid_a[i..] × mid_b[j..], row-major with
        // stride m+1.
        let mut dp = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * (m + 1) + j] = if mid_a[i] == mid_b[j] {
                    dp[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if mid_a[i] == mid_b[j] {
                lines.push(num.stamp(DiffLineKind::Context, mid_a[i]));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1] {
                lines.push(num.stamp(DiffLineKind::Del, mid_a[i]));
                i += 1;
            } else {
                lines.push(num.stamp(DiffLineKind::Add, mid_b[j]));
                j += 1;
            }
        }
        while i < n {
            lines.push(num.stamp(DiffLineKind::Del, mid_a[i]));
            i += 1;
        }
        while j < m {
            lines.push(num.stamp(DiffLineKind::Add, mid_b[j]));
            j += 1;
        }
    }

    for l in &a[end_a..] {
        lines.push(num.stamp(DiffLineKind::Context, l));
    }
    (lines, old_capped || new_capped)
}

/// Side-by-side pairing (req 2.1's split toggle): each Del/Add RUN until
/// the next Context pairs old-left / new-right, the shorter side padded
/// with None; Context rows fill both sides. Pure — the renderer just walks
/// the pairs.
pub fn split_pairs(lines: &[DiffLine]) -> Vec<(Option<DiffLine>, Option<DiffLine>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == DiffLineKind::Context {
            out.push((Some(lines[i].clone()), Some(lines[i].clone())));
            i += 1;
            continue;
        }
        let run = &lines[i..];
        let end = run
            .iter()
            .position(|l| l.kind == DiffLineKind::Context)
            .unwrap_or(run.len());
        let dels: Vec<&DiffLine> = run[..end]
            .iter()
            .filter(|l| l.kind == DiffLineKind::Del)
            .collect();
        let adds: Vec<&DiffLine> = run[..end]
            .iter()
            .filter(|l| l.kind == DiffLineKind::Add)
            .collect();
        for k in 0..dels.len().max(adds.len()) {
            out.push((
                dels.get(k).map(|l| (*l).clone()),
                adds.get(k).map(|l| (*l).clone()),
            ));
        }
        i += end;
    }
    out
}

/// Diff blocks → rendered blocks (TS ToolCard's `diffs.map(DiffBlock)`).
pub fn blocks_of(diffs: &[AcpDiffContent]) -> Vec<DiffBlock> {
    diffs
        .iter()
        .map(|d| {
            let (lines, truncated) = unified(d.old_text.as_deref().unwrap_or(""), &d.new_text);
            DiffBlock {
                path: d.path.clone(),
                lines,
                truncated,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(rows: &[DiffLine]) -> Vec<DiffLineKind> {
        rows.iter().map(|l| l.kind).collect()
    }

    fn text_of(rows: &[DiffLine], kind: DiffLineKind) -> Vec<&str> {
        rows.iter()
            .filter(|l| l.kind == kind)
            .map(|l| l.text.as_str())
            .collect()
    }

    #[test]
    fn middle_edit_keeps_context() {
        let (rows, trunc) = unified("a\nb\nc", "a\nx\nc");
        assert!(!trunc);
        assert_eq!(
            kinds(&rows),
            vec![
                DiffLineKind::Context,
                DiffLineKind::Del,
                DiffLineKind::Add,
                DiffLineKind::Context
            ]
        );
        assert_eq!(text_of(&rows, DiffLineKind::Del), vec!["b"]);
        assert_eq!(text_of(&rows, DiffLineKind::Add), vec!["x"]);
    }

    #[test]
    fn created_file_is_all_adds() {
        let (rows, _) = unified("", "one\ntwo");
        assert_eq!(kinds(&rows), vec![DiffLineKind::Add, DiffLineKind::Add]);
    }

    #[test]
    fn deleted_file_is_all_dels() {
        let (rows, _) = unified("one\ntwo", "");
        assert_eq!(kinds(&rows), vec![DiffLineKind::Del, DiffLineKind::Del]);
    }

    #[test]
    fn common_prefix_and_suffix_skip_the_dp() {
        let old_lines: Vec<String> = (0..50).map(|i| format!("line-{i}")).collect();
        let mut new_lines = old_lines.clone();
        new_lines[24] = "EDITED".to_string();
        let (rows, trunc) = unified(&old_lines.join("\n"), &new_lines.join("\n"));
        assert!(!trunc);
        // 49 context + 1 del + 1 add.
        assert_eq!(rows.len(), 51);
        assert_eq!(text_of(&rows, DiffLineKind::Del), vec!["line-24"]);
        assert_eq!(text_of(&rows, DiffLineKind::Add), vec!["EDITED"]);
    }

    #[test]
    fn oversized_sides_cap_and_flag() {
        let old = "x".repeat(MAX_DIFF_CHARS + 500);
        let (rows, trunc) = unified(&old, "y");
        assert!(trunc);
        assert_eq!(
            kinds(&rows),
            vec![DiffLineKind::Del, DiffLineKind::Add],
            "single-line sides stay one row each after the char cap"
        );
        assert_eq!(rows[0].text.len(), MAX_DIFF_CHARS);
    }

    #[test]
    fn dp_budget_falls_back_to_replace_all() {
        let old: Vec<String> = (0..3000).map(|i| format!("o{i}")).collect();
        let new: Vec<String> = (0..3000).map(|i| format!("n{i}")).collect();
        let (rows, trunc) = unified(&old.join("\n"), &new.join("\n"));
        assert!(!trunc);
        assert_eq!(rows.len(), 6000);
        assert!(rows.iter().take(3000).all(|l| l.kind == DiffLineKind::Del));
        assert!(rows.iter().skip(3000).all(|l| l.kind == DiffLineKind::Add));
    }

    #[test]
    fn blocks_of_carries_path_and_truncation() {
        let diffs = vec![
            AcpDiffContent {
                r#type: "diff".to_string(),
                path: "/w/a.rs".to_string(),
                old_text: None,
                new_text: "fn main() {}".to_string(),
            },
            AcpDiffContent {
                r#type: "diff".to_string(),
                path: "/w/b.rs".to_string(),
                old_text: Some("z".repeat(MAX_DIFF_CHARS + 10)),
                new_text: "short".to_string(),
            },
        ];
        let blocks = blocks_of(&diffs);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].path, "/w/a.rs");
        assert!(!blocks[0].truncated);
        assert_eq!(kinds(&blocks[0].lines), vec![DiffLineKind::Add]);
        assert!(blocks[1].truncated);
        assert_eq!(blocks[1].lines[0].text.len(), MAX_DIFF_CHARS);
    }

    // Req 2.1: 1-based source numbering rides every row.
    #[test]
    fn rows_carry_source_line_numbers() {
        let (rows, _) = unified("a\nb\nc", "a\nx\ny\nc");
        // Context(a:1/1), Del(b:2/-), Add(x:-/2), Add(y:-/3), Context(c:3/4).
        assert_eq!(rows[0].old_no, Some(1));
        assert_eq!(rows[0].new_no, Some(1));
        assert_eq!(rows[1].old_no, Some(2));
        assert_eq!(rows[1].new_no, None);
        assert_eq!(rows[2].old_no, None);
        assert_eq!(rows[2].new_no, Some(2));
        assert_eq!(rows[3].new_no, Some(3));
        assert_eq!(rows[4].old_no, Some(3));
        assert_eq!(rows[4].new_no, Some(4));
    }

    // Req 2.1: the split pairing runs dels left / adds right, padded.
    #[test]
    fn split_pairs_align_runs_side_by_side() {
        let (rows, _) = unified("a\nb\nc\nd", "a\nx\ny\nd");
        let pairs = split_pairs(&rows);
        // Context(a,a), (Del b, Add x), (Del c, Add y), Context(d,d) — the
        // DP may interleave Del/Add rows, but the pairing groups the RUN by
        // kind, so both dels meet both adds.
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[0].0.as_ref().unwrap().text, "a");
        assert_eq!(pairs[0].1.as_ref().unwrap().text, "a");
        assert_eq!(pairs[1].0.as_ref().unwrap().text, "b");
        assert_eq!(pairs[1].1.as_ref().unwrap().text, "x");
        assert_eq!(pairs[2].0.as_ref().unwrap().text, "c");
        assert_eq!(pairs[2].1.as_ref().unwrap().text, "y");
        assert_eq!(pairs[3].0.as_ref().unwrap().text, "d");
        // A longer add run pads the old side with None.
        let (rows, _) = unified("a", "a\np\nq\nr");
        let pairs = split_pairs(&rows);
        // Context(a,a), (None,p), (None,q), (None,r).
        assert_eq!(pairs.len(), 4);
        assert!(pairs[1].0.is_none());
        assert_eq!(pairs[3].1.as_ref().unwrap().text, "r");
    }
}
