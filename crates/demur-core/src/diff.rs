//! Pull request diff ingestion: the input model, unified diff parsing,
//! and ignore-path filtering.

use std::collections::BTreeSet;

use globset::GlobSet;
use globset::GlobSetBuilder;

/// A parsed file-level diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Repository-relative path of the file after the change.
    pub path: String,
    /// Path before the change, when renamed.
    pub old_path: Option<String>,
    /// True when the file is newly added.
    pub is_new: bool,
    /// True when the file is deleted.
    pub is_deleted: bool,
    /// True when the diff is binary and has no hunks.
    pub is_binary: bool,
    /// Hunks in file order.
    pub hunks: Vec<Hunk>,
}

/// One hunk of a file diff with its line ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// First line of the region in the old file, one-based.
    pub old_start: u32,
    /// Number of lines the region spans in the old file.
    pub old_lines: u32,
    /// First line of the region in the new file, one-based.
    pub new_start: u32,
    /// Number of lines the region spans in the new file.
    pub new_lines: u32,
    /// Lines in order.
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// Count of added lines in this hunk.
    pub fn added(&self) -> u32 {
        self.lines
            .iter()
            .filter(|line| line.kind == LineKind::Add)
            .count() as u32
    }

    /// Count of removed lines in this hunk.
    pub fn removed(&self) -> u32 {
        self.lines
            .iter()
            .filter(|line| line.kind == LineKind::Del)
            .count() as u32
    }

    /// True when every changed line only adds or removes whitespace.
    pub fn is_whitespace_only(&self) -> bool {
        self.lines.iter().all(|line| match line.kind {
            LineKind::Context => true,
            LineKind::Add | LineKind::Del => line.content.trim().is_empty(),
        })
    }

    /// Render the hunk in unified diff form for a prompt.
    pub fn render(&self) -> String {
        let mut out = format!(
            "@@ -{},{} +{},{} @@\n",
            self.old_start, self.old_lines, self.new_start, self.new_lines
        );
        for line in &self.lines {
            let prefix = match line.kind {
                LineKind::Context => ' ',
                LineKind::Add => '+',
                LineKind::Del => '-',
            };
            out.push(prefix);
            out.push_str(&line.content);
            out.push('\n');
        }
        out
    }
}

/// The change kind of one diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged context line.
    Context,
    /// Line added by the pull request.
    Add,
    /// Line removed by the pull request.
    Del,
}

/// One line of a hunk with its position in old and new files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// The kind of change.
    pub kind: LineKind,
    /// Line number in the old file, when the line exists there.
    pub old_line: Option<u32>,
    /// Line number in the new file, when the line exists there.
    pub new_line: Option<u32>,
    /// Line content without the diff prefix.
    pub content: String,
}

/// Parse a git unified diff into file diffs. Files with no hunks (mode
/// changes, submodules) are skipped.
pub fn parse_unified_diff(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut hunk: Option<Hunk> = None;
    let mut next_old: u32 = 0;
    let mut next_new: u32 = 0;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(mut file) = current.take() {
                if let Some(finished) = hunk.take() {
                    file.hunks.push(finished);
                }
                files.push(file);
            }
            let path = git_path(rest);
            current = Some(FileDiff {
                path,
                old_path: None,
                is_new: false,
                is_deleted: false,
                is_binary: false,
                hunks: Vec::new(),
            });
        } else if let Some(file) = current.as_mut() {
            if line.starts_with("new file mode") {
                file.is_new = true;
            } else if line.starts_with("deleted file mode") {
                file.is_deleted = true;
            } else if let Some(old) = line.strip_prefix("rename from ") {
                file.old_path = Some(old.to_string());
            } else if let Some(path) = line
                .strip_prefix("+++ b/")
                .or_else(|| line.strip_prefix("+++ "))
                .filter(|path| *path != "/dev/null")
            {
                file.path = path.to_string();
            } else if line.starts_with("Binary files") {
                file.is_binary = true;
            } else if let Some(header) = parse_hunk_header(line) {
                if let Some(finished) = hunk.take() {
                    file.hunks.push(finished);
                }
                next_old = header.0;
                next_new = header.2;
                hunk = Some(Hunk {
                    old_start: header.0,
                    old_lines: header.1,
                    new_start: header.2,
                    new_lines: header.3,
                    lines: Vec::new(),
                });
            } else if let Some(open) = hunk.as_mut() {
                let (kind, content) = match line.chars().next() {
                    Some('+') => (LineKind::Add, &line[1..]),
                    Some('-') => (LineKind::Del, &line[1..]),
                    Some('\\') => continue,
                    _ => (LineKind::Context, line),
                };
                let old_line = if kind != LineKind::Add {
                    let number = next_old;
                    next_old += 1;
                    Some(number)
                } else {
                    None
                };
                let new_line = if kind != LineKind::Del {
                    let number = next_new;
                    next_new += 1;
                    Some(number)
                } else {
                    None
                };
                open.lines.push(DiffLine {
                    kind,
                    old_line,
                    new_line,
                    content: content.to_string(),
                });
            }
        }
    }
    if let Some(mut file) = current.take() {
        if let Some(finished) = hunk.take() {
            file.hunks.push(finished);
        }
        files.push(file);
    }
    for file in &mut files {
        file.hunks.retain(|hunk| !hunk.lines.is_empty());
    }
    files.retain(|file| !file.hunks.is_empty() || file.is_binary);
    files
}

fn git_path(header: &str) -> String {
    let after = header.strip_prefix("a/").unwrap_or(header);
    after
        .split(" b/")
        .next()
        .unwrap_or(after)
        .trim()
        .to_string()
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, new_part) = rest.split_once(" +")?;
    let new_part = new_part.split(" @@").next()?;
    let parse = |part: &str| -> (u32, u32) {
        match part.split_once(',') {
            Some((start, count)) => (start.parse().unwrap_or(0), count.parse().unwrap_or(0)),
            None => (part.parse().unwrap_or(0), 1),
        }
    };
    let (old_start, old_lines) = parse(old_part);
    let (new_start, new_lines) = parse(new_part);
    Some((old_start, old_lines, new_start, new_lines))
}

/// Build a matcher from configured ignore globs. Empty configuration yields
/// a matcher that rejects nothing.
pub fn build_ignore_set(globs: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in globs {
        if let Ok(glob) = globset::Glob::new(pattern) {
            builder.add(glob);
        }
    }
    builder
        .build()
        .expect("glob patterns were validated on add")
}

/// True when the path or any of its parent directories matches the set.
pub fn is_ignored(set: &GlobSet, path: &str) -> bool {
    if set.is_match(path) {
        return true;
    }
    let mut dirs = BTreeSet::new();
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if dirs.insert(parent.to_string()) && set.is_match(parent) {
            return true;
        }
        current = parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    println!(\"extra\");
 }
 // end
diff --git a/Cargo.lock b/Cargo.lock
index 3333333..4444444 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,3 +1,4 @@
+[[package]]
 name = \"demur\"
diff --git a/docs/logo.png b/docs/logo.png
new file mode 100644
index 0000000..5555555
Binary files /dev/null and b/docs/logo.png differ
";

    #[test]
    fn parses_files_hunks_and_lines() {
        let files = parse_unified_diff(FIXTURE);
        assert_eq!(files.len(), 3);
        let main = &files[0];
        assert_eq!(main.path, "src/main.rs");
        assert_eq!(main.hunks.len(), 1);
        let hunk = &main.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines), (1, 4));
        assert_eq!((hunk.new_start, hunk.new_lines), (1, 5));
        assert_eq!(hunk.added(), 2);
        assert_eq!(hunk.removed(), 1);
        let add = hunk
            .lines
            .iter()
            .find(|line| line.kind == LineKind::Add)
            .unwrap();
        assert_eq!(add.new_line, Some(2));
        assert_eq!(add.old_line, None);
    }

    #[test]
    fn parses_binary_files() {
        let files = parse_unified_diff(FIXTURE);
        let logo = &files[2];
        assert_eq!(logo.path, "docs/logo.png");
        assert!(logo.is_new);
        assert!(logo.is_binary);
        assert!(logo.hunks.is_empty());
    }

    #[test]
    fn added_and_deleted_files_are_flagged() {
        let diff = "\
diff --git a/new.rs b/new.rs
new file mode 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,1 @@
+fn fresh() {}
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-fn stale() {}
";
        let files = parse_unified_diff(diff);
        assert!(files[0].is_new);
        assert!(files[1].is_deleted);
        assert_eq!(files[1].hunks[0].added(), 0);
        assert_eq!(files[1].hunks[0].removed(), 1);
    }

    #[test]
    fn whitespace_only_hunks_are_detected() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,2 @@
-    
+\t
";
        let files = parse_unified_diff(diff);
        assert!(files[0].hunks[0].is_whitespace_only());
    }

    #[test]
    fn render_round_trips_a_hunk() {
        let files = parse_unified_diff(FIXTURE);
        let rendered = files[0].hunks[0].render();
        assert!(rendered.starts_with("@@ -1,4 +1,5 @@"));
        assert!(rendered.contains("+    println!(\"new\");"));
        assert!(rendered.contains("-    println!(\"old\");"));
    }

    #[test]
    fn ignore_paths_match_files_and_parents() {
        let set = build_ignore_set(&["**/Cargo.lock".to_string(), "dist".to_string()]);
        assert!(is_ignored(&set, "crates/app/Cargo.lock"));
        assert!(is_ignored(&set, "dist/bundle.js"));
        assert!(!is_ignored(&set, "src/distinct.rs"));
        assert!(!is_ignored(&set, "src/main.rs"));
    }
}
