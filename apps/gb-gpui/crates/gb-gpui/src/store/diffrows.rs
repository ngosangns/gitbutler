//! Unified-diff rows. Built once per diff, off the UI thread.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Hunk,
    Add,
    Delete,
    Context,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
    pub hunk: Option<usize>,
    pub lane: Option<String>,
}

/// Turn one patch into fixed rows.
///
/// `max_lines` is how many content rows to keep. A longer patch ends with
/// `Diff too large — showing first N lines`. A binary file is one `Binary file`
/// row. GitButler's too-large patch (no body) uses the same sentence with N = 0.
pub fn build_diff_rows(
    _path: &str,
    binary: bool,
    too_large: bool,
    patch: &str,
    hunk_lanes: &[Option<String>],
    max_lines: usize,
) -> Vec<DiffRow> {
    if binary {
        return vec![note("Binary file")];
    }
    if too_large && patch.is_empty() {
        return vec![note("Diff too large — showing first 0 lines")];
    }
    let mut rows = Vec::new();
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut hunk_index: Option<usize> = None;
    let mut lane: Option<String> = None;
    let mut seen_hunks = 0usize;
    for line in patch.split('\n') {
        if line.is_empty() || skipped(line) {
            continue;
        }
        if let Some((old_start, _old_lines, new_start, _new_lines)) = hunk_header(line) {
            old_no = old_start;
            new_no = new_start;
            lane = hunk_lanes.get(seen_hunks).cloned().flatten();
            hunk_index = Some(seen_hunks);
            seen_hunks += 1;
            if !push(
                &mut rows,
                max_lines,
                hunk_row(line, hunk_index, lane.clone()),
            ) {
                break;
            }
            continue;
        }
        let (kind, text, old_line, new_line) = if let Some(text) = line.strip_prefix('+') {
            let row = (RowKind::Add, text.to_string(), None, Some(new_no));
            new_no = new_no.saturating_add(1);
            row
        } else if let Some(text) = line.strip_prefix('-') {
            let row = (RowKind::Delete, text.to_string(), Some(old_no), None);
            old_no = old_no.saturating_add(1);
            row
        } else if line.starts_with('\\') {
            (RowKind::Context, line.to_string(), None, None)
        } else {
            let text = line.strip_prefix(' ').unwrap_or(line).to_string();
            let row = (RowKind::Context, text, Some(old_no), Some(new_no));
            old_no = old_no.saturating_add(1);
            new_no = new_no.saturating_add(1);
            row
        };
        let row = DiffRow {
            kind,
            old_line,
            new_line,
            text,
            hunk: hunk_index,
            lane: lane.clone(),
        };
        if !push(&mut rows, max_lines, row) {
            break;
        }
    }
    rows
}

fn push(rows: &mut Vec<DiffRow>, max_lines: usize, row: DiffRow) -> bool {
    if rows.len() >= max_lines {
        rows.push(note(format!(
            "Diff too large — showing first {max_lines} lines"
        )));
        return false;
    }
    rows.push(row);
    true
}

fn note(text: impl Into<String>) -> DiffRow {
    DiffRow {
        kind: RowKind::Note,
        old_line: None,
        new_line: None,
        text: text.into(),
        hunk: None,
        lane: None,
    }
}

fn hunk_row(line: &str, hunk: Option<usize>, lane: Option<String>) -> DiffRow {
    DiffRow {
        kind: RowKind::Hunk,
        old_line: None,
        new_line: None,
        text: line.to_string(),
        hunk,
        lane,
    }
}

fn skipped(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "diff --git",
        "index ",
        "--- ",
        "+++ ",
        "new file",
        "deleted file",
        "similarity ",
        "rename ",
        "old mode",
        "new mode",
        "Binary files",
    ];
    PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

/// `@@ -start[,count] +start[,count] @@`. A missing count means 1.
fn hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ ")?;
    let (old, rest) = rest.split_once(' ')?;
    let new = rest.split_once(' ').map(|(new, _)| new).unwrap_or(rest);
    let old = old.strip_prefix('-')?;
    let new = new.strip_prefix('+')?;
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Some((old_start, old_lines, new_start, new_lines))
}

fn range(text: &str) -> Option<(u32, u32)> {
    match text.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn rows_for_a_modified_file_keep_numbers_and_lanes() {
        let patch = "\
diff --git a/src/app.rs b/src/app.rs
--- a/src/app.rs
+++ b/src/app.rs
@@ -1,3 +1,4 @@
 fn main() {
-    old();
+    new();
+    extra();
 }
";
        let rows = build_diff_rows(
            "src/app.rs",
            false,
            false,
            patch,
            &[Some("lane-a".to_string())],
            100,
        );
        assert_eq!(rows[0].kind, RowKind::Hunk);
        assert_eq!(rows[0].lane.as_deref(), Some("lane-a"));
        assert_eq!(rows[0].text, "@@ -1,3 +1,4 @@");
        let added: Vec<_> = rows
            .iter()
            .filter(|row| row.kind == RowKind::Add)
            .map(|row| row.text.as_str())
            .collect();
        assert_eq!(added, ["    new();", "    extra();"]);
        let deleted = rows.iter().find(|row| row.kind == RowKind::Delete).unwrap();
        assert_eq!(deleted.old_line, Some(2));
        assert_eq!(deleted.new_line, None);
        let first_add = rows.iter().find(|row| row.kind == RowKind::Add).unwrap();
        assert_eq!(first_add.new_line, Some(2));
    }

    #[test]
    fn rows_for_a_new_file_and_an_omitted_count() {
        let patch = "@@ -0,0 +1,2 @@\n+fn view() {\n+}\n@@ -1 +1,3 @@\n context\n";
        let rows = build_diff_rows("src/new_view.rs", false, false, patch, &[None, None], 100);
        assert_eq!(rows[0].kind, RowKind::Hunk);
        assert_eq!(rows[1].kind, RowKind::Add);
        assert_eq!(rows[1].new_line, Some(1));
        assert_eq!(rows[1].old_line, None);
        let second = rows
            .iter()
            .find(|row| row.text.starts_with("@@ -1 "))
            .unwrap();
        assert_eq!(second.kind, RowKind::Hunk);
        let context = rows.iter().find(|row| row.text == "context").unwrap();
        assert_eq!(context.old_line, Some(1));
        assert_eq!(context.new_line, Some(1));
    }

    #[test]
    fn a_binary_file_is_one_placeholder_row() {
        let rows = build_diff_rows("assets/logo.bin", true, false, "", &[], 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "Binary file");
        assert_eq!(rows[0].kind, RowKind::Note);
    }

    #[test]
    fn a_long_diff_stops_at_the_cap() {
        let mut patch = String::from("@@ -0,0 +1,30 @@\n");
        for index in 0..30 {
            patch.push_str(&format!("+line {index}\n"));
        }
        let rows = build_diff_rows("big.rs", false, false, &patch, &[None], 5);
        assert_eq!(rows.len(), 6);
        assert_eq!(
            rows.last().unwrap().text,
            "Diff too large — showing first 5 lines"
        );
        let empty = build_diff_rows("big.rs", false, true, "", &[], 5);
        assert_eq!(empty[0].text, "Diff too large — showing first 0 lines");
    }

    #[test]
    fn fifty_thousand_lines_build_in_reasonable_time() {
        let mut patch = String::with_capacity(50_000 * 12);
        patch.push_str("@@ -0,0 +1,50000 @@\n");
        for index in 0..50_000 {
            patch.push('+');
            patch.push_str(&index.to_string());
            patch.push('\n');
        }
        let started = Instant::now();
        let rows = build_diff_rows("huge.rs", false, false, &patch, &[None], 100_000);
        let elapsed = started.elapsed();
        assert!(rows.len() > 50_000, "{}", rows.len());
        assert!(
            elapsed.as_secs() < 2,
            "row builder took {elapsed:?} for 50000 lines"
        );
    }
}
