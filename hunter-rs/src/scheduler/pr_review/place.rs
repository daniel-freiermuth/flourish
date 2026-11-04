//! Where on a PR's diff an issue can go inline. A forge takes a review
//! comment only on a line inside one of the diff's hunks: GitHub rejects
//! the whole review otherwise, GitLab files the note against nothing. So
//! an issue is posted inline only when its line is in a hunk of the PR's
//! unified diff ([`Forge::pr_diff`](crate::forge::Forge::pr_diff)), and in
//! the review's body otherwise.

use crate::forge::Side;

/// A line of a hunk an issue was placed on, as a forge's position needs
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The file's path in the base, and in the head (the base's path for a
    /// deleted file: the head has none).
    pub old_path: String,
    pub new_path: String,
    /// The line's number in the base and in the head, where it has one:
    /// an unchanged line has both, a removed one only the base's, an added
    /// one only the head's.
    pub old_line: Option<i64>,
    pub new_line: Option<i64>,
}

/// Where line `line` of `file`, numbered in the head (`Side::Right`) or in
/// the base (`Side::Left`), lies inside a hunk of the unified `diff`, with
/// both of its numbers; `None` when it is in no hunk.
///
/// `file` matches a file of the diff by its head or its base path (a
/// renamed file is named either way), with a stray leading `./`, `a/` or
/// `b/` tolerated: a reviewer may copy a path out of the diff's headers.
pub fn place(diff: &str, file: &str, line: i64, side: Side) -> Option<Located> {
    let wanted = file.trim().trim_start_matches("./");
    let stripped = wanted
        .strip_prefix("a/")
        .or_else(|| wanted.strip_prefix("b/"));
    let mut current: Option<FileOfDiff> = None;
    // The hunk being read: the next line's number in the base and in the
    // head, and how many lines of each the header said are left.
    let mut hunk: Option<Hunk> = None;
    for text in diff.lines() {
        if let Some(h) = hunk.as_mut().filter(|h| h.old_left > 0 || h.new_left > 0) {
            let (in_old, in_new) = match text.as_bytes().first() {
                Some(b'+') => (false, true),
                Some(b'-') => (true, false),
                Some(b'\\') => continue,
                // A context line; an empty one is a blank line whose
                // leading space a tool trimmed.
                _ => (true, true),
            };
            let at = (in_old.then_some(h.old), in_new.then_some(h.new));
            if in_old {
                h.old += 1;
                h.old_left -= 1;
            }
            if in_new {
                h.new += 1;
                h.new_left -= 1;
            }
            let hit = match side {
                Side::Right => at.1 == Some(line),
                Side::Left => at.0 == Some(line),
            };
            if hit
                && let Some(f) = current
                    .as_ref()
                    .filter(|f| f.names(wanted) || stripped.is_some_and(|s| f.names(s)))
            {
                return Some(Located {
                    old_path: f.old_path.clone(),
                    new_path: f.new_path.clone(),
                    old_line: at.0,
                    new_line: at.1,
                });
            }
            continue;
        }
        hunk = None;
        if let Some(rest) = text.strip_prefix("diff --git ") {
            current = Some(FileOfDiff::from_git_header(rest));
        } else if let Some(h) = text.strip_prefix("@@ ") {
            hunk = Hunk::parse(h);
        } else if let Some(f) = current.as_mut() {
            if let Some(p) = text.strip_prefix("rename from ") {
                p.clone_into(&mut f.old_path);
            } else if let Some(p) = text.strip_prefix("rename to ") {
                p.clone_into(&mut f.new_path);
            } else if let Some(p) = text.strip_prefix("--- a/") {
                p.clone_into(&mut f.old_path);
            } else if let Some(p) = text.strip_prefix("+++ b/") {
                p.clone_into(&mut f.new_path);
            }
        }
    }
    None
}

/// The paths of the file whose hunks are being read.
struct FileOfDiff {
    old_path: String,
    new_path: String,
}

impl FileOfDiff {
    /// From what follows `diff --git `: `a/<old> b/<new>`. A path with a
    /// space is ambiguous here; the `---`/`+++` and rename lines that
    /// follow say it again unambiguously and take over. A deleted file's
    /// `+++ /dev/null` and an added one's `--- /dev/null` change nothing,
    /// so each keeps the header's path on that side.
    fn from_git_header(rest: &str) -> Self {
        let (old, new) = rest
            .rfind(" b/")
            .map_or((rest, rest), |i| (&rest[..i], &rest[i + 3..]));
        let old = old.strip_prefix("a/").unwrap_or(old);
        Self {
            old_path: old.to_owned(),
            new_path: new.to_owned(),
        }
    }

    fn names(&self, path: &str) -> bool {
        self.new_path == path || self.old_path == path
    }
}

/// A hunk header's position, counted down as its lines are read.
struct Hunk {
    old: i64,
    new: i64,
    old_left: i64,
    new_left: i64,
}

impl Hunk {
    /// From what follows `@@ `: `-<start>[,<count>] +<start>[,<count>] @@`.
    fn parse(header: &str) -> Option<Self> {
        let mut parts = header.split_whitespace();
        let (old, old_left) = range(parts.next()?.strip_prefix('-')?)?;
        let (new, new_left) = range(parts.next()?.strip_prefix('+')?)?;
        Some(Self {
            old,
            new,
            old_left,
            new_left,
        })
    }
}

/// `<start>[,<count>]`; the count is 1 when left out.
fn range(s: &str) -> Option<(i64, i64)> {
    match s.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(diff: &str, file: &str, line: i64, side: Side) -> bool {
        place(diff, file, line, side).is_some()
    }

    /// `src/lib.rs`: line 10 changed, line 12 added (head numbering: 10
    /// changed, 11 context, 12 new); a second hunk further down.
    const MODIFIED: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -9,3 +9,4 @@ fn top() {
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;
+let d = 5;
@@ -40,2 +41,2 @@ fn bottom() {
-old_tail();
+new_tail();
 end();
";

    #[test]
    fn a_line_inside_a_hunk_is_placed_and_one_outside_is_not() {
        assert!(placed(MODIFIED, "src/lib.rs", 10, Side::Right));
        assert!(placed(MODIFIED, "src/lib.rs", 12, Side::Right));
        // Context lines are in the hunk too.
        assert!(placed(MODIFIED, "src/lib.rs", 9, Side::Right));
        assert!(!placed(MODIFIED, "src/lib.rs", 13, Side::Right));
        assert!(!placed(MODIFIED, "src/lib.rs", 1, Side::Right));
        assert!(!placed(MODIFIED, "src/other.rs", 10, Side::Right));
    }

    #[test]
    fn the_side_chooses_which_version_numbers_the_line() {
        // Base line 10 is the removed `let b = 2;`; base line 12 is past
        // the hunk's three base lines, though head line 12 is in it.
        assert_eq!(
            place(MODIFIED, "src/lib.rs", 10, Side::Left),
            Some(Located {
                old_path: "src/lib.rs".into(),
                new_path: "src/lib.rs".into(),
                old_line: Some(10),
                new_line: None,
            })
        );
        assert!(!placed(MODIFIED, "src/lib.rs", 12, Side::Left));
        // Head line 11 is context: both numbers, the base's being 11 too.
        assert_eq!(
            place(MODIFIED, "src/lib.rs", 11, Side::Right).map(|l| (l.old_line, l.new_line)),
            Some((Some(11), Some(11)))
        );
    }

    #[test]
    fn every_hunk_of_a_file_counts() {
        // Second hunk: base 40-41, head 41-42.
        assert!(placed(MODIFIED, "src/lib.rs", 41, Side::Right));
        assert!(placed(MODIFIED, "src/lib.rs", 42, Side::Right));
        assert!(placed(MODIFIED, "src/lib.rs", 40, Side::Left));
        assert!(!placed(MODIFIED, "src/lib.rs", 30, Side::Right));
        assert!(!placed(MODIFIED, "src/lib.rs", 43, Side::Right));
    }

    #[test]
    fn an_added_file_has_only_head_lines() {
        let diff = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
";
        assert_eq!(
            place(diff, "new.txt", 2, Side::Right),
            Some(Located {
                old_path: "new.txt".into(),
                new_path: "new.txt".into(),
                old_line: None,
                new_line: Some(2),
            })
        );
        assert!(!placed(diff, "new.txt", 3, Side::Right));
        assert!(!placed(diff, "new.txt", 1, Side::Left));
    }

    #[test]
    fn a_deleted_file_has_only_base_lines() {
        let diff = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
";
        assert_eq!(
            place(diff, "gone.txt", 2, Side::Left),
            Some(Located {
                old_path: "gone.txt".into(),
                new_path: "gone.txt".into(),
                old_line: Some(2),
                new_line: None,
            })
        );
        assert!(!placed(diff, "gone.txt", 1, Side::Right));
    }

    #[test]
    fn a_renamed_file_is_found_by_either_path() {
        let diff = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 90%
rename from old/name.rs
rename to new/name.rs
--- a/old/name.rs
+++ b/new/name.rs
@@ -1,2 +1,2 @@
-fn a() {}
+fn b() {}
 fn c() {}
";
        assert_eq!(
            place(diff, "new/name.rs", 1, Side::Right).map(|at| (at.old_path, at.new_path)),
            Some(("old/name.rs".to_owned(), "new/name.rs".to_owned()))
        );
        // The path as the diff's `+++ b/` header spells it.
        assert!(placed(diff, "b/new/name.rs", 1, Side::Right));
        assert!(placed(diff, "old/name.rs", 1, Side::Left));
        assert!(!placed(diff, "name.rs", 1, Side::Right));
    }

    #[test]
    fn a_hunk_body_line_that_looks_like_a_header_is_still_body() {
        // A removed line `-- a/x` reads as `--- a/x` in the diff.
        let diff = "\
diff --git a/x.sql b/x.sql
--- a/x.sql
+++ b/x.sql
@@ -1,2 +1,1 @@
--- a/x
 keep
";
        assert!(placed(diff, "x.sql", 1, Side::Left));
        assert!(placed(diff, "x.sql", 1, Side::Right));
    }
}
