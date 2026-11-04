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
                && let Some((old_path, new_path)) = current.as_ref().and_then(FileOfDiff::paths)
                && (names(old_path, new_path, wanted)
                    || stripped.is_some_and(|s| names(old_path, new_path, s)))
            {
                return Some(Located {
                    old_path: old_path.to_owned(),
                    new_path: new_path.to_owned(),
                    old_line: at.0,
                    new_line: at.1,
                });
            }
            continue;
        }
        hunk = None;
        if text.starts_with("diff --git ") {
            current = Some(FileOfDiff::default());
        } else if let Some(h) = text.strip_prefix("@@ ") {
            hunk = Hunk::parse(h);
        } else if let Some(f) = current.as_mut() {
            if let Some(p) = text.strip_prefix("rename from ") {
                f.old_path = Some(git_path(p));
            } else if let Some(p) = text.strip_prefix("rename to ") {
                f.new_path = Some(git_path(p));
            } else if let Some(p) = text.strip_prefix("--- ") {
                f.old_path = side_path(p, "a/");
            } else if let Some(p) = text.strip_prefix("+++ ") {
                f.new_path = side_path(p, "b/");
            }
        }
    }
    None
}

/// Whether a file of the diff with these paths is the one `path` names.
fn names(old_path: &str, new_path: &str, path: &str) -> bool {
    new_path == path || old_path == path
}

/// The paths of the file whose hunks are being read, each `None` until a
/// line names that side, or once one said the side does not exist.
///
/// git names each side on a line of its own, so without ambiguity: in a
/// rename's `rename from` and `rename to`, and in the `---`/`+++` pair
/// every file with hunks has before its first, where `/dev/null` says the
/// side does not exist: an added file has no base, a deleted one no head.
/// The `diff --git` header names both sides too, but an unquoted path
/// holding ` b/` makes it ambiguous, and the lines after it always name
/// each side before a hunk: a file starts with neither. A side that does
/// not exist takes the other's path ([`Self::paths`]): a forge names a
/// deleted file by its base path on both sides, an added one by its head
/// path.
#[derive(Default)]
struct FileOfDiff {
    old_path: Option<String>,
    new_path: Option<String>,
}

impl FileOfDiff {
    /// The base's path and the head's, a side that does not exist taking
    /// the other's; `None` while no line named either.
    fn paths(&self) -> Option<(&str, &str)> {
        let old = self.old_path.as_deref().or(self.new_path.as_deref())?;
        let new = self.new_path.as_deref().or(self.old_path.as_deref())?;
        Some((old, new))
    }
}

/// The path a `---` (`prefix` `a/`) or `+++` (`b/`) line names after its
/// marker, `None` for `/dev/null`: the side does not exist.
fn side_path(s: &str, prefix: &str) -> Option<String> {
    let path = git_path(s);
    (path != "/dev/null").then(|| strip_side(path, prefix))
}

/// `path` without git's side prefix `prefix` (`a/` or `b/`).
fn strip_side(path: String, prefix: &str) -> String {
    path.strip_prefix(prefix).map(str::to_owned).unwrap_or(path)
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

/// The path at the start of `s` as git writes one in a diff's headers. A
/// path git quoted (`"a/caf\303\251.txt"`) is unquoted by C's rules, its
/// octal escapes being the bytes of its UTF-8. An unquoted one runs to the
/// end, or to a tab: git ends the `---`/`+++` name of a path with a space
/// in one, as GNU patch expects.
fn git_path(s: &str) -> String {
    let Some(quoted) = s.strip_prefix('"') else {
        return s.split_once('\t').map_or(s, |(path, _)| path).to_owned();
    };
    let mut bytes = Vec::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return String::from_utf8_lossy(&bytes).into_owned(),
            '\\' => {
                let Some(e) = chars.next() else { break };
                let byte = match e {
                    'a' => 0x07,
                    'b' => 0x08,
                    'f' => 0x0c,
                    'n' => b'\n',
                    'r' => b'\r',
                    't' => b'\t',
                    'v' => 0x0b,
                    '0'..='3' => {
                        let mut n = u32::from(e) - u32::from('0');
                        for _ in 0..2 {
                            let d = chars.next().and_then(|d| d.to_digit(8));
                            n = n * 8 + d.unwrap_or(0);
                        }
                        u8::try_from(n).unwrap_or(b'?')
                    }
                    other => {
                        let mut buf = [0; 4];
                        bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                        continue;
                    }
                };
                bytes.push(byte);
            }
            c => {
                let mut buf = [0; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    // No closing quote: not git's quoting after all, the line as written.
    s.to_owned()
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

    /// git ends the `---`/`+++` name of a path with a space in a tab, and
    /// C-quotes a path with a non-ASCII character in every header (as
    /// `git diff` prints them, checked against git 2.56).
    #[test]
    fn paths_git_quotes_or_ends_in_a_tab_are_read_as_git_writes_them() {
        let spaced = "\
diff --git a/my file.txt b/my file.txt
--- a/my file.txt\t
+++ b/my file.txt\t
@@ -1 +1 @@
-one
+two
";
        assert!(placed(spaced, "my file.txt", 1, Side::Right));
        assert!(placed(spaced, "my file.txt", 1, Side::Left));
        let quoted = "\
diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"
new file mode 100644
--- /dev/null
+++ \"b/caf\\303\\251.txt\"
@@ -0,0 +1 @@
+un
";
        assert_eq!(
            place(quoted, "café.txt", 1, Side::Right).map(|at| (at.old_path, at.new_path)),
            Some(("café.txt".to_owned(), "café.txt".to_owned()))
        );
        let renamed = "\
diff --git a/plain.txt \"b/tab\\there.txt\"
similarity index 90%
rename from plain.txt
rename to \"tab\\there.txt\"
--- a/plain.txt
+++ \"b/tab\\there.txt\"
@@ -1 +1 @@
-a
+b
";
        assert_eq!(
            place(renamed, "tab\there.txt", 1, Side::Right).map(|at| (at.old_path, at.new_path)),
            Some(("plain.txt".to_owned(), "tab\there.txt".to_owned()))
        );
    }

    /// An unquoted path holding ` b/` makes `diff --git` ambiguous to
    /// split at a ` b/`; each side is still named on its own lines, and a
    /// side that does not exist takes the other's path (as `git diff`
    /// prints them, checked against git 2.56).
    #[test]
    fn a_path_holding_b_slash_is_read_from_the_lines_naming_each_side() {
        let deleted = "\
diff --git a/docs/x b/y.txt b/docs/x b/y.txt
deleted file mode 100644
index 01e79c3..0000000
--- a/docs/x b/y.txt\t
+++ /dev/null
@@ -1,3 +0,0 @@
-1
-2
-3
";
        assert_eq!(
            place(deleted, "docs/x b/y.txt", 3, Side::Left).map(|at| (at.old_path, at.new_path)),
            Some(("docs/x b/y.txt".to_owned(), "docs/x b/y.txt".to_owned()))
        );
        assert!(!placed(deleted, "y.txt", 3, Side::Left));
        let added = "\
diff --git a/new b/w.txt b/new b/w.txt
new file mode 100644
index 0000000..e556b83
--- /dev/null
+++ b/new b/w.txt\t
@@ -0,0 +1 @@
+w
";
        assert_eq!(
            place(added, "new b/w.txt", 1, Side::Right).map(|at| (at.old_path, at.new_path)),
            Some(("new b/w.txt".to_owned(), "new b/w.txt".to_owned()))
        );
        let renamed = "\
diff --git a/new b/w.txt b/new b/v b/z.txt
similarity index 50%
rename from new b/w.txt
rename to new b/v b/z.txt
index e556b83..f00ba12 100644
--- a/new b/w.txt\t
+++ b/new b/v b/z.txt\t
@@ -1 +1 @@
-w
+z
";
        assert_eq!(
            place(renamed, "new b/v b/z.txt", 1, Side::Right).map(|at| (at.old_path, at.new_path)),
            Some(("new b/w.txt".to_owned(), "new b/v b/z.txt".to_owned()))
        );
    }

    /// git's `\ No newline at end of file` follows the last line of a side
    /// that has no newline: a marker, no line of either side, so the lines
    /// after it keep their numbers.
    #[test]
    fn a_no_newline_marker_is_no_line_of_either_side() {
        let diff = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        assert_eq!(
            place(diff, "f.txt", 1, Side::Right),
            Some(Located {
                old_path: "f.txt".into(),
                new_path: "f.txt".into(),
                old_line: None,
                new_line: Some(1),
            })
        );
        assert_eq!(
            place(diff, "f.txt", 1, Side::Left).map(|at| (at.old_line, at.new_line)),
            Some((Some(1), None))
        );
        assert!(!placed(diff, "f.txt", 2, Side::Right));
    }
}
