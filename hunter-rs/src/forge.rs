//! Forge abstraction — GitHub / GitLab PR/MR lifecycle via CLI tools.
//! Port of hunter/forge.py. Methods shell out to `gh` / `glab`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::clones::{Force, Unheld};
use crate::domain::ForgeName;
use crate::util::{run_cmd, run_cmd_in, run_cmd_input};

/// Git forge PR/MR lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrState {
    #[default]
    Open,
    Merged,
    Closed,
}

impl PrState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
        }
    }
}

/// Whether the PR can be merged cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mergeable {
    #[default]
    Unknown,
    Mergeable,
    Conflicting,
}

impl Mergeable {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mergeable => "MERGEABLE",
            Self::Conflicting => "CONFLICTING",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// Review decision on the PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewDecision {
    #[default]
    None,
    Approved,
    ChangesRequested,
}

/// Check/CI conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConclusion {
    Success,
    Failure,
    TimedOut,
    Cancelled,
    Neutral,
    Skipped,
    Pending,
    Other,
}

impl CheckConclusion {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "SUCCESS" => Self::Success,
            // A legacy status's `ERROR` and a check run that never started
            // (`STARTUP_FAILURE`, e.g. an invalid workflow) are red CI too.
            "FAILURE" | "ERROR" | "STARTUP_FAILURE" => Self::Failure,
            "TIMED_OUT" => Self::TimedOut,
            "CANCELLED" => Self::Cancelled,
            "NEUTRAL" => Self::Neutral,
            "SKIPPED" => Self::Skipped,
            "PENDING" | "" => Self::Pending,
            _ => Self::Other,
        }
    }
    pub fn is_failing(self) -> bool {
        matches!(self, Self::Failure | Self::TimedOut | Self::Cancelled)
    }
    pub fn is_passing(self) -> bool {
        matches!(self, Self::Success | Self::Neutral | Self::Skipped)
    }
}

// ---------------------------------------------------------------------------
// Typed API response structs
// ---------------------------------------------------------------------------

/// Author in GitHub API responses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GhAuthor {
    #[serde(default)]
    pub login: String,
}

/// Comment from GitHub's `pr.comments` array, or one left on a line of
/// the diff (GitHub's review comments, GitLab's `DiffNote`s), which
/// carries its [`CodeLocation`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhComment {
    /// GraphQL node id: what [`github_screen`] asks the author's account
    /// type by.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub author: Option<GhAuthor>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
    /// Where in the diff it was left; `None` for the PR's conversation.
    #[serde(skip)]
    pub location: Option<CodeLocation>,
    /// Set by [`screen_feedback`]; never read from the forge.
    #[serde(skip)]
    pub voice: Voice,
}

/// The file and line an inline comment was left on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeLocation {
    pub path: String,
    /// `None` for a comment on the whole file.
    pub line: Option<i64>,
    /// The line is in a version of the file the PR has since changed:
    /// the comment may no longer apply where it says.
    pub outdated: bool,
    /// The line is one the PR removes: `line` numbers it in the PR's base,
    /// and the head no longer has it.
    pub removed: bool,
}

/// Review from GitHub's `pr.reviews` array.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhReview {
    /// GraphQL node id: what [`github_screen`] asks the author's account
    /// type by.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub author: Option<GhAuthor>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub submitted_at: String,
    #[serde(default)]
    pub state: String,
    /// Set by [`screen_feedback`]; never read from the forge.
    #[serde(skip)]
    pub voice: Voice,
}

/// Status check / CI check entry.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GhCheckRun {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

// -- GitLab API types (normalized into Gh* types at parse boundary) ----------
//
// GitLab sends `null` for any field it has no value for -- an MR's empty
// description, a fresh MR's `sha` and `diff_refs` shas -- and
// `#[serde(default)]` alone covers only a missing key: on `null` the whole
// payload fails to parse. So every field of a GitLab payload is either an
// `Option` or read through [`nullable`], which takes `null` for the
// field's default.

/// Deserialize a GitLab payload field that may be `null` as its type's
/// default (see the GitLab API types above).
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlAuthor {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default, deserialize_with = "nullable")]
    pub username: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlNote {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default, deserialize_with = "nullable")]
    pub body: String,
    /// Whether the note's thread is resolved; every note of a discussion
    /// carries it.
    #[serde(default, deserialize_with = "nullable")]
    pub resolved: bool,
    #[serde(default)]
    pub author: Option<GlAuthor>,
    #[serde(default, deserialize_with = "nullable")]
    pub created_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub updated_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub system: bool,
    #[serde(default, rename = "type")]
    pub note_type: Option<String>,
    /// Where a `DiffNote` sits in the diff.
    #[serde(default)]
    pub position: Option<GlPosition>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlPosition {
    #[serde(default)]
    pub new_path: Option<String>,
    #[serde(default)]
    pub old_path: Option<String>,
    /// `None` on a removed line, which only `old_line` numbers.
    #[serde(default)]
    pub new_line: Option<i64>,
    #[serde(default)]
    pub old_line: Option<i64>,
}

impl GlPosition {
    fn location(&self) -> CodeLocation {
        // A removed line exists only in the old file, so only old_path
        // matches the number old_line gives it.
        let removed = self.new_line.is_none() && self.old_line.is_some();
        let (path, other) = if removed {
            (&self.old_path, &self.new_path)
        } else {
            (&self.new_path, &self.old_path)
        };
        CodeLocation {
            path: path.clone().or_else(|| other.clone()).unwrap_or_default(),
            line: self.new_line.or(self.old_line),
            outdated: false,
            removed,
        }
    }
}

/// One element of GitHub's `pulls/{n}/comments`: a comment on a line of
/// the diff, which `gh pr view` does not return at all.
#[derive(Debug, Default, Deserialize)]
struct GhReviewComment {
    /// The comment's id, in the order GitHub created the comments.
    #[serde(default)]
    id: i64,
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    user: Option<GhAuthor>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    path: String,
    /// Null once the commented line is gone from the PR's head.
    #[serde(default)]
    line: Option<i64>,
    #[serde(default)]
    original_line: Option<i64>,
    /// `LEFT` for a line the PR removes, which the line fields number in
    /// the base.
    #[serde(default)]
    side: Option<String>,
}

impl From<GhReviewComment> for GhComment {
    fn from(c: GhReviewComment) -> Self {
        Self {
            id: c.node_id,
            author: c.user,
            body: c.body,
            created_at: c.created_at,
            location: Some(CodeLocation {
                path: c.path,
                line: c.line.or(c.original_line),
                outdated: c.line.is_none() && c.original_line.is_some(),
                removed: c.side.as_deref() == Some("LEFT"),
            }),
            voice: Voice::default(),
        }
    }
}

/// Every inline comment on GitHub PR `pr_number` of the repo at `url`,
/// oldest first, unscreened.
fn github_review_comments(url: &str, pr_number: i64) -> anyhow::Result<Vec<GhComment>> {
    let (host, path) =
        url_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let api = format!("repos/{path}/pulls/{pr_number}/comments?per_page=100");
    // `--jq` runs per page, so `.[]` turns every page into one stream of
    // objects rather than `--paginate`'s concatenated arrays.
    let mut argv = vec!["gh", "api", "--paginate", &api, "--jq", ".[]"];
    if !host.eq_ignore_ascii_case("github.com") {
        argv.extend(["--hostname", host]);
    }
    let (rc, out) = run_cmd(&argv, 60);
    if rc != 0 {
        anyhow::bail!("gh api {api} failed (rc={rc}): {out}");
    }
    serde_json::Deserializer::from_str(&out)
        .into_iter::<GhReviewComment>()
        .map(|c| Ok(c?.into()))
        .collect::<Result<_, serde_json::Error>>()
        .map_err(|e| anyhow::anyhow!("gh api {api}: unparseable review comments: {e}"))
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GlPipeline {
    #[serde(default, deserialize_with = "nullable")]
    status: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GlMergeRequest {
    #[serde(default, deserialize_with = "nullable")]
    state: String,
    #[serde(default)]
    detailed_merge_status: Option<String>,
    #[serde(default)]
    merge_status: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    source_branch: String,
    #[serde(default, deserialize_with = "nullable")]
    sha: String,
    #[serde(default, deserialize_with = "nullable")]
    updated_at: String,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "nullable")]
    description: String,
    #[serde(default)]
    head_pipeline: Option<GlPipeline>,
    #[serde(default)]
    pipeline: Option<GlPipeline>,
    #[serde(default, deserialize_with = "nullable")]
    target_branch: String,
    #[serde(default)]
    author: Option<GlAuthor>,
    #[serde(default)]
    source_project_id: Option<i64>,
    #[serde(default)]
    target_project_id: Option<i64>,
    #[serde(default, deserialize_with = "nullable")]
    iid: i64,
    #[serde(default, deserialize_with = "nullable")]
    draft: bool,
    #[serde(default, deserialize_with = "nullable")]
    web_url: String,
    /// The shas a diff position on the MR's current version names.
    #[serde(default)]
    diff_refs: Option<GlDiffRefs>,
}

/// `diff_refs` of a GitLab MR: the commits its current diff is between.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "GitLab's own field names, deserialized as sent"
)]
struct GlDiffRefs {
    #[serde(default, deserialize_with = "nullable")]
    base_sha: String,
    #[serde(default, deserialize_with = "nullable")]
    start_sha: String,
    #[serde(default, deserialize_with = "nullable")]
    head_sha: String,
}

/// What a review of somebody else's PR needs to know about it
/// ([`Forge::view_pr_meta`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrMeta {
    pub state: PrState,
    pub head_sha: String,
    pub head_ref: String,
    /// The branch the PR merges into.
    pub base_ref: String,
    /// The author's forge login.
    pub author: String,
    pub title: String,
    pub body: String,
    /// Whether the head lives in a fork rather than the repo itself.
    pub cross_repo: bool,
    /// The login owning the fork the head lives in (a user, or a group or
    /// organization); empty for a head in the repo itself, and when the
    /// forge names no owner (the fork is gone). A PR's author may open it
    /// from somebody else's fork, whose code a review then runs.
    pub head_owner: String,
}

impl PrMeta {
    /// The owner of the fork the head lives in, whose repository's code a
    /// round runs; `None` for a head in the repo itself, which only those
    /// who can push to it write. `Some("")` when the forge names none.
    pub fn fork_owner(&self) -> Option<&str> {
        self.cross_repo.then_some(self.head_owner.as_str())
    }
}

/// One open PR of a repo ([`Forge::list_open_prs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub is_draft: bool,
    pub cross_repo: bool,
    pub url: String,
}

/// Which version of a file a review comment's line numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    /// The PR's head: added and unchanged lines.
    #[default]
    Right,
    /// The base: lines the PR removes (and unchanged ones).
    Left,
}

impl Side {
    /// The word GitHub's review API and `pr_review_issues.side` use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Right => "RIGHT",
            Self::Left => "LEFT",
        }
    }
}

/// One issue of a [`ReviewPost`] placed on a line of the PR's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineComment {
    /// The `pr_review_issues` row this comment carries.
    pub issue_id: i64,
    /// The file's path in the head (in the base, for a deleted file).
    pub path: String,
    /// The file's path in the base: `path`, unless the PR renamed it.
    pub old_path: String,
    /// The line as the reviewer gave it, numbered on `side`.
    pub line: i64,
    pub side: Side,
    /// The line's number in the base and in the head, where it has one: an
    /// unchanged line has both, which GitLab's position needs.
    pub old_line: Option<i64>,
    pub new_line: Option<i64>,
    pub body: String,
}

/// The most characters GitHub takes in a review's body, and in each of its
/// comments: a longer one is refused (422, "body is too long") on every
/// attempt alike. GitLab takes 1,000,000 in a note, so this bounds what
/// hunter posts either forge.
pub const REVIEW_TEXT_MAX_CHARS: usize = 65_536;

/// A COMMENT review as [`Forge::post_review`] sends it: posts go out under
/// the operator's forge login, so hunter never approves or requests
/// changes in their name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPost {
    /// The head the review read; the comments' lines number its diff.
    pub commit_id: String,
    /// Markdown: the body's first lines.
    pub header: String,
    /// The issues no comment carries, each as the body shows it
    /// ([`Self::push_unplaced`]), in order.
    pub unplaced: Vec<String>,
    /// The body's last line: the step mark the review is found by
    /// ([`step_mark`]).
    pub mark: String,
    pub comments: Vec<InlineComment>,
}

impl ReviewPost {
    /// Add to the body an issue that is not on a line of the diff, at
    /// `location` (`file:line`) if it names one.
    pub fn push_unplaced(&mut self, location: Option<&str>, text: &str) {
        let mut item = String::from("\n\n---\n\n");
        if let Some(at) = location {
            item.push('`');
            item.push_str(at);
            item.push_str("`\n\n");
        }
        item.push_str(text);
        self.unplaced.push(item);
    }

    /// The body [`Forge::post_review`] sends, before the own-comment mark
    /// it adds ([`mark_own`]): the header, then the unplaced issues in
    /// order for as long as the whole stays within
    /// [`REVIEW_TEXT_MAX_CHARS`], a line counting those left out, and the
    /// step mark last. Every issue in a body past the forge's limit would
    /// fail the post on every attempt; the marks, which a retry and the
    /// read-back find the review by, are never cut.
    pub fn body(&self) -> String {
        let left_out = |n: usize| {
            format!(
                "\n\n---\n\n_{n} more issue(s) left out: a review's body holds at most \
                 {REVIEW_TEXT_MAX_CHARS} characters._"
            )
        };
        let chars = |s: &str| s.chars().count();
        // What follows the issues, at its longest: the count of those left
        // out, the step mark, and the own-comment mark.
        let tail =
            chars(&left_out(self.unplaced.len())) + chars(&self.mark) + chars(&mark_own("")) + 2;
        let room = REVIEW_TEXT_MAX_CHARS.saturating_sub(tail);
        let mut body = self.header.clone();
        let mut used = chars(&body);
        let mut shown = 0;
        for item in &self.unplaced {
            let n = chars(item);
            if used + n > room {
                break;
            }
            body.push_str(item);
            used += n;
            shown += 1;
        }
        if shown < self.unplaced.len() {
            body.push_str(&left_out(self.unplaced.len() - shown));
        }
        body.push_str("\n\n");
        body.push_str(&self.mark);
        body
    }

    /// This post with every inline comment moved into the body: the lines
    /// they name are not where the forge would put them (the PR moved on
    /// since the review read it).
    #[must_use]
    pub fn without_inline(mut self) -> Self {
        for c in std::mem::take(&mut self.comments) {
            let at = format!(
                "{}:{}{}",
                c.path,
                c.line,
                if c.side == Side::Left { " (base)" } else { "" }
            );
            self.push_unplaced(Some(&at), &c.body);
        }
        self
    }
}

/// What [`Forge::post_review`] posted: the forge's id of the review, and
/// the issues whose comment went inline. The inline comments' own ids are
/// read back from the PR afterwards ([`Forge::inline_comment_refs`]): a
/// read that fails then leaves the review up and only the read to retry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostedReview {
    pub review_ref: String,
    pub inline: Vec<i64>,
}

/// What an earlier post of a review left on the PR, as
/// [`Forge::find_review`] reads it: the review itself, by the forge login
/// and carrying the post's step mark, and the issues whose inline comment
/// the login has there (GitLab publishes them one by one, before the body;
/// on GitHub they go up with their review or not at all).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoundReview {
    pub review_ref: Option<String>,
    pub inline: Vec<i64>,
}

/// What a failed [`Forge::post_review`] already put on the PR: GitLab
/// publishes a review's notes one by one, so a failure can come after some
/// inline comments are out. Their issues, as [`PostedReview::inline`]
/// names them; the post's error carries it as context, so the retry leaves
/// them out instead of posting them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartlyPosted {
    pub inline: Vec<i64>,
}

impl std::fmt::Display for PartlyPosted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} inline comment(s) published before the failure",
            self.inline.len()
        )
    }
}

/// A failed [`Forge::post_review`] put none of the review on the PR: it
/// failed before the call that may put it there, or the forge refused that
/// call outright ([`refused_outright`]). The post's error carries it as
/// context. Without it, a failed post may have landed all the same (a
/// timeout, a 5xx after the write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NothingPosted;

impl std::fmt::Display for NothingPosted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing of the review was posted")
    }
}

/// Whether a failed `gh api` or `glab api` call's output, `out`, is the
/// forge refusing the request: both end an HTTP error's message with
/// `(HTTP <status>)`, and a 4xx answers a request the forge did not act
/// on. A timeout or a 5xx says nothing of what the forge did.
fn refused_outright(out: &str) -> bool {
    out.contains("(HTTP 4")
}

/// The thread one of hunter's inline review comments opened, as
/// [`Forge::review_threads`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThread {
    /// The forge's id of the thread's first comment, hunter's: what
    /// [`Forge::inline_comment_refs`] read back for the issue.
    pub first_ref: String,
    /// What resolving takes: GitHub's review thread node id, GitLab's
    /// discussion id.
    pub thread_id: String,
    /// What replying takes ([`Forge::reply_to_thread`]): GitHub's database
    /// id of the thread's first comment (empty if GitHub gave none),
    /// GitLab's discussion id.
    pub reply_to: String,
    pub resolved: bool,
    /// The replies after hunter's comment, oldest first, screened to the
    /// authors a review trusts ([`screen_replies`]).
    pub replies: Vec<ThreadReply>,
    /// Replies the screen dropped.
    pub hidden: usize,
}

/// One screened reply of a [`ReviewThread`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReply {
    /// The author's login, as the forge spells it; [`OWN_AUTHOR`] for one
    /// of hunter's own replies.
    pub author: String,
    /// As written; hunter's own without the own-comment mark and the step
    /// mark.
    pub body: String,
    /// When it went out on the forge, as the forge spells it (RFC 3339;
    /// [`ThreadComment::published`]): a reply newer than any round has
    /// seen makes a watched review due.
    pub published_at: String,
    /// Whether it is hunter's own reply. The only way to tell one apart:
    /// a person's login may be `hunter` too.
    pub own: bool,
    /// The step of a post it is ([`step_mark`]), when it is hunter's own
    /// and carries one.
    pub step: Option<String>,
}

/// A comment in one of hunter's review threads, as the forge lists it, for
/// [`screen_replies`].
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadComment {
    #[serde(default)]
    id: String,
    /// GitHub's database id (a `BigInt`, sent as a string): what its REST
    /// API takes to reply under the comment. `None` for GitLab's notes.
    #[serde(default)]
    full_database_id: Option<String>,
    #[serde(default)]
    author: Option<GhAuthor>,
    #[serde(default)]
    body: String,
    /// When the comment was published: a comment written in a pending
    /// review is created when drafted, and published only when the review
    /// is submitted, any time later. GitHub's `publishedAt`, null for a
    /// comment still in a pending review; for GitLab, whose draft notes
    /// become notes only as they are published, the note's creation.
    #[serde(default)]
    published_at: Option<String>,
}

impl ThreadComment {
    /// When the comment became visible on the PR; `None` while it is a
    /// draft nobody else can see (only the viewer's own drafts are
    /// returned, i.e. the operator's). A draft is no reply yet: counting
    /// it would make a review due, and get it answered, before anyone
    /// could read it, and again once it is submitted. Dating a reply by
    /// its drafting could also put it behind a watermark set meanwhile.
    fn published(&self) -> Option<&str> {
        self.published_at.as_deref()
    }
}

/// The author a reply of hunter's own is shown under. Never compared to
/// tell whose a reply is ([`ThreadReply::own`]).
pub const OWN_AUTHOR: &str = "hunter";

/// Who may answer in the thread of a reviewed PR besides those who can
/// push and the configured bots ([`screen_replies`]).
#[derive(Debug, Clone, Copy)]
struct ThreadVoices<'a> {
    /// The PR's author: the request vouched for them, so they may argue
    /// with the review in their own PR.
    pr_author: &'a str,
    /// Who wrote the thread's first comment: hunter's forge login, which
    /// can push (it must, to every repo it tracks) and is known without
    /// asking. The only login whose replies can be hunter's own: the
    /// own-comment mark is text anybody can type.
    poster: &'a str,
}

/// Keep the replies in one of hunter's review threads whose author a
/// review trusts: a configured bot (confirmed by `is_bot`, as in
/// [`screen_feedback`]), hunter's own login, anyone `can_push` vouches
/// for, and the PR's author. The rest are only counted. A reply ending in
/// [`OWN_MARK`] by hunter's login (the thread's poster) is hunter's own
/// ([`ThreadReply::own`]), shown as [`OWN_AUTHOR`]; everybody else keeps
/// their login, a pusher who typed the mark included: a reply passed off
/// as hunter's would be read as hunter's answer, not as a claim to check.
/// Each is dated by when it went out ([`ThreadComment::published`]).
/// `can_push` is asked at most once per login; an error fails the whole
/// read, as guessing either way would.
fn screen_replies(
    replies: Vec<ThreadComment>,
    voices: ThreadVoices<'_>,
    bots: &ReviewBots,
    mut is_bot: impl FnMut(&str, &str) -> anyhow::Result<bool>,
    mut can_push: impl FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<(Vec<ThreadReply>, usize)> {
    let poster = normalize_login(voices.poster);
    let pr_author = normalize_login(voices.pr_author);
    let mut push_access: HashMap<String, bool> = HashMap::new();
    let mut kept = Vec::with_capacity(replies.len());
    let mut hidden = 0;
    for c in replies {
        // A draft is nobody's reply yet: not kept, and not hidden either.
        let Some(published_at) = c.published().map(str::to_owned) else {
            continue;
        };
        let Some(login) = c
            .author
            .as_ref()
            .map(|a| a.login.clone())
            .filter(|l| !l.is_empty())
        else {
            hidden += 1;
            continue;
        };
        let normal = normalize_login(&login);
        if bots.contains(&login) && is_bot(&c.id, &login)? {
            kept.push(ThreadReply {
                author: login,
                body: c.body,
                published_at,
                own: false,
                step: None,
            });
            continue;
        }
        let poster_wrote = !poster.is_empty() && normal == poster;
        if poster_wrote && is_own(&c.body) {
            let body = c.body.trim_end();
            let body = body.strip_suffix(OWN_MARK).unwrap_or(body).trim_end();
            let (body, step) = split_step_mark(body);
            kept.push(ThreadReply {
                author: OWN_AUTHOR.to_owned(),
                body: body.to_owned(),
                published_at,
                own: true,
                step: step.map(str::to_owned),
            });
            continue;
        }
        let trusted = poster_wrote || normal == pr_author || {
            if let Some(&known) = push_access.get(&normal) {
                known
            } else {
                let asked = can_push(&login)?;
                push_access.insert(normal.clone(), asked);
                asked
            }
        };
        if trusted {
            kept.push(ThreadReply {
                author: login,
                body: c.body,
                published_at,
                own: false,
                step: None,
            });
        } else {
            hidden += 1;
        }
    }
    Ok((kept, hidden))
}

/// One file of `merge_requests/:iid/diffs`: GitLab returns the hunks
/// without the file headers, which [`gitlab_unified_diff`] puts back.
#[derive(Debug, Clone, Default, Deserialize)]
// The bools are GitLab's own independent per-file flags, deserialized
// as sent; folding them into an enum would invent states the API lacks.
#[allow(clippy::struct_excessive_bools)]
struct GlFileDiff {
    #[serde(default, deserialize_with = "nullable")]
    old_path: String,
    #[serde(default, deserialize_with = "nullable")]
    new_path: String,
    #[serde(default, deserialize_with = "nullable")]
    new_file: bool,
    #[serde(default, deserialize_with = "nullable")]
    deleted_file: bool,
    #[serde(default, deserialize_with = "nullable")]
    diff: String,
    /// GitLab withholds the hunks of a file over its diff size limits
    /// (`too_large`) or past its per-MR display limits (`collapsed`) and
    /// sends an empty `diff` for it; without these the file reads as
    /// changing nothing.
    #[serde(default, deserialize_with = "nullable")]
    too_large: bool,
    #[serde(default, deserialize_with = "nullable")]
    collapsed: bool,
}

/// PR view data returned by `view_pr_sync` and `view_pr_engage`.
///
/// `comments` and `reviews` hold only screened feedback: whatever
/// [`screen_feedback`] let through, each item tagged with its [`Voice`].
#[derive(Debug, Clone, Default)]
pub struct PrView {
    pub state: PrState,
    pub mergeable: Mergeable,
    pub review_decision: ReviewDecision,
    pub head_ref: String,
    pub head_sha: String,
    pub updated_at: String,
    pub title: String,
    pub body: String,
    pub status_check_rollup: Vec<GhCheckRun>,
    pub comments: Vec<GhComment>,
    pub reviews: Vec<GhReview>,
    /// Part of the feedback could not be read (GitLab's notes, which the
    /// sync view tolerates failing): `comments` is not the whole thread,
    /// and the newest activity on the PR is unknown, not none.
    pub feedback_unread: bool,
}

/// Whose feedback a comment or review is.
///
/// Hunter's PRs sit on repos where anyone may comment, and engage acts on
/// what it reads: it commits, and the scheduler pushes. So the forge drops
/// every comment and review whose author cannot steer the PR before a
/// [`PrView`] leaves it ([`screen_feedback`]); what remains carries one
/// of these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Voice {
    /// Someone who can push to the repository.
    #[default]
    Maintainer,
    /// A review bot the operator listed in `feedback.bots`: acted on, but
    /// read critically, and never mistaken for a maintainer's decision.
    Bot,
    /// Hunter itself: a comment carrying [`OWN_MARK`]. Told apart by its
    /// body, not its author, because hunter may post under the operator's
    /// own login. Never new activity -- it is hunter's answer, not
    /// feedback -- but kept, so a later engage sees what it already said.
    Own,
}

/// Appended to every comment hunter posts ([`mark_own`]): an HTML comment,
/// so it does not render, and both forges return it verbatim in the raw
/// body they hand back.
pub const OWN_MARK: &str = "<!-- posted by hunter -->";

/// `body` as hunter posts it, with [`OWN_MARK`] on a line of its own.
pub fn mark_own(body: &str) -> String {
    format!("{body}\n\n{OWN_MARK}")
}

/// The hidden line naming one step of a review post, `step` (a round's
/// review, its answer in a thread), put in what the step posts. A forge
/// call can fail after the forge acted on it (a timeout, a 5xx after the
/// write): a later read finds the step by this line before taking it again
/// ([`Forge::find_review`], [`ThreadReply::step`]).
pub fn step_mark(step: &str) -> String {
    format!("{STEP_MARK_OPEN}{step}{STEP_MARK_CLOSE}")
}

const STEP_MARK_OPEN: &str = "<!-- hunter step: ";
const STEP_MARK_CLOSE: &str = " -->";

/// Whether `line` is the step mark of `step` ([`step_mark`]).
fn is_step_mark(line: &str, step: &str) -> bool {
    line.trim()
        .strip_prefix(STEP_MARK_OPEN)
        .and_then(|rest| rest.strip_suffix(STEP_MARK_CLOSE))
        == Some(step)
}

/// `body` (hunter's own, without [`OWN_MARK`]) without the step mark on
/// its last line, and the step it names, if it carries one.
fn split_step_mark(body: &str) -> (&str, Option<&str>) {
    let (rest, last) = body.rsplit_once('\n').unwrap_or(("", body));
    let step = last
        .trim()
        .strip_prefix(STEP_MARK_OPEN)
        .and_then(|s| s.strip_suffix(STEP_MARK_CLOSE));
    match step {
        Some(step) => (rest.trim_end(), Some(step)),
        None => (body, None),
    }
}

/// The review bots whose feedback is let through (`feedback.bots`).
///
/// Held normalised -- lowercase, without GitHub's `[bot]` suffix -- since
/// `gh pr view` names an app `coderabbitai` where the REST API says
/// `coderabbitai[bot]`, and an operator may copy either.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewBots(Vec<String>);

impl ReviewBots {
    pub fn new<S: AsRef<str>>(logins: impl IntoIterator<Item = S>) -> Self {
        Self(
            logins
                .into_iter()
                .map(|l| normalize_login(l.as_ref()))
                .collect(),
        )
    }

    pub fn contains(&self, login: &str) -> bool {
        let login = normalize_login(login);
        self.0.contains(&login)
    }
}

fn normalize_login(login: &str) -> String {
    let login = login.trim().to_ascii_lowercase();
    match login.strip_suffix("[bot]") {
        Some(bare) => bare.to_owned(),
        None => login,
    }
}

/// Keep only the feedback whose author may steer the PR, tagging each
/// item with its [`Voice`]: configured bots first (a bot that can push is
/// still a bot), then whoever `can_push` vouches for. Everything else --
/// strangers, deleted accounts, items without an author -- is dropped, so
/// it can neither reach a prompt nor raise a PR's attention.
///
/// An item by a login on the bot list counts as the bot's only once
/// `is_bot` (given the item's id and login) confirms its author is one: a
/// login is just a name, and a human who holds the same name must not be
/// read as the bot. Unconfirmed, it is screened like anyone else's.
///
/// `can_push` is asked at most once per distinct login. An error fails
/// the whole screen: guessing either way would drop a maintainer's
/// request or hand a stranger's text to the worker.
///
/// A maintainer's item that ends in [`OWN_MARK`] is hunter's own
/// ([`Voice::Own`]). Only a maintainer's: the mark is plain text anyone
/// can type, so it must not let a stranger's comment through.
fn screen_feedback(
    pr: &mut PrView,
    bots: &ReviewBots,
    mut is_bot: impl FnMut(&str, &str) -> anyhow::Result<bool>,
    mut can_push: impl FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut push_access: HashMap<String, bool> = HashMap::new();
    let mut voice_of = |id: &str, author: Option<&GhAuthor>| -> anyhow::Result<Option<Voice>> {
        let Some(login) = author.map(|a| a.login.as_str()).filter(|l| !l.is_empty()) else {
            return Ok(None);
        };
        if bots.contains(login) && is_bot(id, login)? {
            return Ok(Some(Voice::Bot));
        }
        let maintainer = if let Some(&known) = push_access.get(login) {
            known
        } else {
            let asked = can_push(login)?;
            push_access.insert(login.to_owned(), asked);
            asked
        };
        Ok(maintainer.then_some(Voice::Maintainer))
    };
    let own = |voice: Voice, body: &str| {
        if voice == Voice::Maintainer && is_own(body) {
            Voice::Own
        } else {
            voice
        }
    };
    let mut comments = Vec::with_capacity(pr.comments.len());
    for mut c in std::mem::take(&mut pr.comments) {
        if let Some(voice) = voice_of(&c.id, c.author.as_ref())? {
            c.voice = own(voice, &c.body);
            comments.push(c);
        }
    }
    let mut reviews = Vec::with_capacity(pr.reviews.len());
    for mut r in std::mem::take(&mut pr.reviews) {
        if let Some(voice) = voice_of(&r.id, r.author.as_ref())? {
            r.voice = own(voice, &r.body);
            reviews.push(r);
        }
    }
    pr.comments = comments;
    pr.reviews = reviews;
    Ok(())
}

/// Whether `body` is one hunter posted: its last line is exactly
/// [`OWN_MARK`], as [`mark_own`] leaves it. A reply quoting hunter's
/// comment carries the mark only behind a `> `, so it stays the quoter's.
fn is_own(body: &str) -> bool {
    body.trim_end().lines().next_back().map(str::trim) == Some(OWN_MARK)
}

/// How long a lookup answer is reused. `sync_prs` screens every open PR
/// on every cycle, which would otherwise cost one API call per commenter
/// per PR per cycle; the price is that a revoked collaborator is still
/// believed for up to this long.
const LOOKUP_TTL: Duration = Duration::from_mins(15);

/// Screening answers by `<question>:<host>/<project>:<user>` (or
/// `:<item>` for an author's type), with the time each was recorded. Time
/// is passed in rather than read, so the expiry boundary is testable.
#[derive(Default)]
struct LookupCache(HashMap<String, (bool, Instant)>);

impl LookupCache {
    /// The answer for `key` if it was recorded less than [`LOOKUP_TTL`]
    /// before `now`.
    fn fresh(&self, key: &str, now: Instant) -> Option<bool> {
        let &(answer, at) = self.0.get(key)?;
        (now.saturating_duration_since(at) < LOOKUP_TTL).then_some(answer)
    }

    fn record(&mut self, key: String, answer: bool, now: Instant) {
        self.record_all([(key, answer)], now);
    }

    /// Record a batch of answers. Recording also drops every answer that
    /// has expired by `now` -- once per batch, not per answer -- so the
    /// cache holds what was asked in the last [`LOOKUP_TTL`], not every
    /// comment and review the daemon has ever screened.
    fn record_all(&mut self, answers: impl IntoIterator<Item = (String, bool)>, now: Instant) {
        self.0
            .retain(|_, &mut (_, at)| now.saturating_duration_since(at) < LOOKUP_TTL);
        self.0.extend(
            answers
                .into_iter()
                .map(|(key, answer)| (key, (answer, now))),
        );
    }
}

static LOOKUPS: LazyLock<Mutex<LookupCache>> = LazyLock::new(Mutex::default);

/// `look_up`'s answer for `key`, reused for [`LOOKUP_TTL`]. Errors are
/// not cached, so the next screen asks again. The lock is not held across
/// `look_up`, a subprocess that may take seconds.
fn cached_lookup(
    key: String,
    look_up: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    if let Some(answer) = LOOKUPS
        .lock()
        .ok()
        .and_then(|cache| cache.fresh(&key, Instant::now()))
    {
        return Ok(answer);
    }
    let answer = look_up()?;
    if let Ok(mut cache) = LOOKUPS.lock() {
        cache.record(key, answer, Instant::now());
    }
    Ok(answer)
}

/// Whether `login` may go into a GitHub API path: GitHub's own alphabet,
/// plus the brackets of an app's `name[bot]` and the underscore of
/// Enterprise managed users. Nothing else can be an account.
fn github_login_is_wellformed(login: &str) -> bool {
    login
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'[' | b']'))
}

/// `gh api <api> [--hostname host] --jq <jq>`, for the repo at `url`.
fn github_api_jq(url: &str, api: &str, jq: &str) -> anyhow::Result<(i32, String)> {
    let (host, _) =
        url_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse host from {url}"))?;
    let mut argv = vec!["gh", "api", api, "--jq", jq];
    if !host.eq_ignore_ascii_case("github.com") {
        argv.extend(["--hostname", host]);
    }
    Ok(run_cmd(&argv, 30))
}

/// The ids, among `ids` (GraphQL node ids of comments and reviews), whose
/// author is a GitHub App (`Bot`).
///
/// `gh pr view` names an app by its bare slug (`greptile-apps`, where REST
/// says `greptile-apps[bot]`), and that bare name is an ordinary account
/// name too: a person who holds it writes comments that read exactly like
/// the bot's (github.com has a `User` called `greptile-apps`). The login
/// cannot tell them apart; the author's type, asked per item, can. An item
/// GitHub no longer returns (deleted meanwhile) is not the bot's.
///
/// An item's author never changes, so answers are cached per item and one
/// GraphQL request covers up to 100 of the rest.
fn github_bot_authored(url: &str, host: &str, ids: &[&str]) -> anyhow::Result<HashSet<String>> {
    let key = |id: &str| format!("github-bot-item:{host}:{id}");
    let mut bot = HashSet::new();
    let mut ask = Vec::new();
    {
        let now = Instant::now();
        let cache = LOOKUPS.lock().ok();
        for &id in ids {
            match cache.as_ref().and_then(|c| c.fresh(&key(id), now)) {
                Some(true) => {
                    bot.insert(id.to_owned());
                }
                Some(false) => {}
                None => ask.push(id),
            }
        }
    }
    for chunk in ask.chunks(100) {
        let types = github_author_types(url, chunk)?;
        let answers: Vec<(String, bool)> = chunk
            .iter()
            .map(|&id| (key(id), types.get(id).is_some_and(|t| t == "Bot")))
            .collect();
        bot.extend(
            chunk
                .iter()
                .zip(&answers)
                .filter(|(_, (_, is_bot))| *is_bot)
                .map(|(&id, _)| id.to_owned()),
        );
        if let Ok(mut cache) = LOOKUPS.lock() {
            cache.record_all(answers, Instant::now());
        }
    }
    Ok(bot)
}

/// Author `__typename` (`Bot`, `User`, ...) of each comment or review in
/// `ids`, by id, in one GraphQL request.
fn github_author_types(url: &str, ids: &[&str]) -> anyhow::Result<HashMap<String, String>> {
    const QUERY: &str = "query($ids: [ID!]!) { nodes(ids: $ids) { id \
                         ... on Comment { author { __typename } } } }";
    const JQ: &str = r#".data.nodes[] | select(. != null) | "\(.id)\t\(.author.__typename // "")""#;
    let (host, _) =
        url_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse host from {url}"))?;
    let query = format!("query={QUERY}");
    let id_args: Vec<String> = ids.iter().map(|id| format!("ids[]={id}")).collect();
    let mut argv = vec!["gh", "api", "graphql", "-f", &query];
    for arg in &id_args {
        argv.extend(["-f", arg]);
    }
    argv.extend(["--jq", JQ]);
    if !host.eq_ignore_ascii_case("github.com") {
        argv.extend(["--hostname", host]);
    }
    let (rc, out) = run_cmd(&argv, 30);
    if rc != 0 {
        anyhow::bail!("gh api graphql (author types) failed (rc={rc}): {out}");
    }
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(id, kind)| (id.to_owned(), kind.trim().to_owned()))
        .collect())
}

/// Whether `login` can push to the GitHub repo at `url`: the collaborator
/// permission endpoint's `user.permissions.push`, true for write,
/// maintain and admin and for custom roles built on them. On a public
/// repo everyone else reads as `read`, and a login GitHub no longer knows
/// (a deleted account) is a 404 -- a "no", not an error. GitHub answers
/// only a caller who can push to the repo itself; hunter's login must be
/// one (it pushes its fixes there, and every trust check rests on this
/// lookup), and the error says so when it is not.
fn github_can_push(url: &str, login: &str) -> anyhow::Result<bool> {
    if !github_login_is_wellformed(login) {
        return Ok(false);
    }
    let (_, path) =
        url_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let api = format!("repos/{path}/collaborators/{login}/permission");
    let (rc, out) = github_api_jq(url, &api, ".user.permissions.push")?;
    if rc != 0 {
        if out.contains("(HTTP 404)") {
            return Ok(false);
        }
        if out.contains("Must have push access") {
            anyhow::bail!(
                "hunter's forge login cannot push to {path}, so GitHub will not say who else \
                 can: hunter needs push access to every repo it tracks, reviewed ones included"
            );
        }
        anyhow::bail!("gh api {api} failed (rc={rc}): {out}");
    }
    match out.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => anyhow::bail!("gh api {api}: unexpected push permission {other:?}"),
    }
}

/// GitLab's Developer role, the lowest that can push to a branch.
const GITLAB_DEVELOPER: i64 = 30;

/// Whether GitLab user `user_id` can push to `project`: a member, direct
/// or inherited from a group, at Developer or above. A non-member is a
/// 404 -- a "no", not an error.
fn gitlab_can_push(url: &str, project: &str, user_id: i64) -> anyhow::Result<bool> {
    #[derive(Deserialize)]
    struct Member {
        access_level: i64,
    }
    let enc = project.replace('/', "%2F");
    let api = format!("projects/{enc}/members/all/{user_id}");
    let (rc, out) = gitlab_api(url, &api, None, &[]);
    if rc != 0 {
        if out.contains("(HTTP 404)") {
            return Ok(false);
        }
        anyhow::bail!("glab api {api} failed (rc={rc}): {out}");
    }
    let member: Member = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("glab api {api}: unparseable member: {e}"))?;
    Ok(member.access_level >= GITLAB_DEVELOPER)
}

/// The lowercase host of the GitHub repo at `url` and `<host>/<path>`:
/// what the lookup cache keys answers about it by.
fn github_cache_scope(url: &str) -> (String, String) {
    url_host_path(url).map_or_else(
        || (String::new(), url.to_owned()),
        |(host, path)| {
            let host = host.to_ascii_lowercase();
            let repo = format!("{host}/{path}");
            (host, repo)
        },
    )
}

/// Screen a GitHub PR view: see [`screen_feedback`].
fn github_screen(url: &str, pr: &mut PrView, bots: &ReviewBots) -> anyhow::Result<()> {
    let (host, repo) = github_cache_scope(url);
    // Only items a listed bot's login wrote need their author's type.
    let ids: Vec<&str> = pr
        .comments
        .iter()
        .map(|c| (c.id.as_str(), c.author.as_ref()))
        .chain(
            pr.reviews
                .iter()
                .map(|r| (r.id.as_str(), r.author.as_ref())),
        )
        .filter(|(id, a)| !id.is_empty() && a.is_some_and(|a| bots.contains(&a.login)))
        .map(|(id, _)| id)
        .collect();
    let bot_authored = if ids.is_empty() {
        HashSet::new()
    } else {
        github_bot_authored(url, &host, &ids)?
    };
    screen_feedback(
        pr,
        bots,
        |id, _| Ok(bot_authored.contains(id)),
        |login| {
            cached_lookup(
                format!("github-push:{repo}:{}", normalize_login(login)),
                || github_can_push(url, login),
            )
        },
    )
}

/// A GitLab MR's notes as screened comments: see
/// [`screen_feedback`]. Membership is looked up by user id, which the
/// notes carry and the normalised Gh types do not.
///
/// A configured bot needs no further confirmation here: a GitLab username
/// names exactly one account on its instance -- bots are ordinary user
/// accounts -- unlike GitHub, where an app's bare slug is also free for a
/// person to hold.
fn gitlab_feedback(
    url: &str,
    project: &str,
    notes: &[GlNote],
    bots: &ReviewBots,
) -> anyhow::Result<Vec<GhComment>> {
    let ids: HashMap<&str, i64> = notes
        .iter()
        .filter_map(|n| n.author.as_ref())
        .filter_map(|a| Some((a.username.as_str(), a.id?)))
        .collect();
    let mut view = PrView {
        comments: gitlab_split_notes(notes),
        ..PrView::default()
    };
    let host = gitlab_host_path(url)
        .map_or("", |(h, _)| h)
        .to_ascii_lowercase();
    screen_feedback(
        &mut view,
        bots,
        |_, _| Ok(true),
        |login| {
            // Membership is looked up by id; a note that names its author
            // without one leaves who wrote it unknown, which is not the
            // same as known to be an outsider.
            let Some(&id) = ids.get(login) else {
                anyhow::bail!("GitLab note by {login:?} carries no author id");
            };
            cached_lookup(format!("gitlab-push:{host}/{project}:{id}"), || {
                gitlab_can_push(url, project, id)
            })
        },
    )?;
    Ok(view.comments)
}

pub trait Forge: Send + Sync {
    /// owner/repo from a remote URL.
    fn owner_repo(&self, url: &str) -> Option<(String, String)>;
    /// Extract (`owner_repo_slug`, `pr_number`) from a PR/MR web URL.
    fn parse_pr_url(&self, url: &str) -> Option<(String, i64)>;
    /// Create a draft PR/MR. Returns the PR URL.
    fn create_pr(
        &self,
        repo_path: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<String>;
    /// Lightweight PR view for `sync_prs` (state, checks, mergeable), its
    /// feedback screened ([`screen_feedback`]).
    fn view_pr_sync(&self, url: &str, pr_number: i64, bots: &ReviewBots) -> anyhow::Result<PrView>;
    /// [`Self::view_pr_sync`] for a caller that must see every comment, or
    /// fail: checking whether hunter's own post already landed before
    /// posting it again. GitLab's sync view tolerates a failed notes read
    /// with an empty list; this one fails instead.
    fn view_pr_comments(
        &self,
        url: &str,
        pr_number: i64,
        bots: &ReviewBots,
    ) -> anyhow::Result<PrView> {
        self.view_pr_sync(url, pr_number, bots)
    }
    /// Heavier PR view for engage and harvest: title/body, and every
    /// inline comment (with its [`CodeLocation`]) among `comments`, its
    /// feedback screened ([`screen_feedback`]). Sync does without GitHub's
    /// inline comments: each belongs to a review, whose timestamp already
    /// raises `new_comments`.
    fn view_pr_engage(
        &self,
        url: &str,
        pr_number: i64,
        bots: &ReviewBots,
    ) -> anyhow::Result<PrView>;
    /// The PR/MR's unified diff: what it proposed. A closed PR's changes
    /// never reach the default branch, so this is the only place a worker
    /// reviewing it can read them.
    fn pr_diff(&self, url: &str, pr_number: i64) -> anyhow::Result<String>;
    /// Post a comment on a PR/MR, marked as hunter's own ([`mark_own`]) so
    /// that screening never reads it back as feedback.
    fn comment_pr(&self, url: &str, pr_number: i64, body: &str) -> anyhow::Result<()>;
    /// Close a PR/MR with a comment explaining why.
    fn close_pr(&self, url: &str, pr_number: i64, comment: &str) -> anyhow::Result<()>;
    /// Force-push `HEAD` to `branch` at `url`, the repo's URL as stored:
    /// the transport it is cloned over is the one it is pushed over.
    fn push(&self, repo_path: &Path, url: &str, branch: &str) -> anyhow::Result<()>;
    /// What a review of PR `pr_number` needs: state, head, base, author,
    /// title and body, and whether the head is in a fork.
    fn view_pr_meta(&self, url: &str, pr_number: i64) -> anyhow::Result<PrMeta>;
    /// The repo's open PRs (one page of up to 100).
    fn list_open_prs(&self, url: &str) -> anyhow::Result<Vec<OpenPr>>;
    /// Whether `login` can push to the repo at `url`: the one definition
    /// of a trusted author. Cached like feedback screening's lookups. Only
    /// a login that can push may ask, which hunter's must: GitHub answers
    /// nobody else, and on GitLab, which answers any member, hunter's own
    /// access is checked first, so a request fails saying so on either.
    fn can_push(&self, url: &str, login: &str) -> anyhow::Result<bool>;
    /// The web URL of PR `pr_number` of the repo at `url`.
    fn pr_web_url(&self, url: &str, pr_number: i64) -> String;
    /// The fetch refspec that brings PR `pr_number`'s head, fork or not,
    /// into the clone as `refs/hunter/pr/<n>` ([`pr_head_ref`]).
    fn pr_head_refspec(&self, pr_number: i64) -> String;
    /// Post `post` on PR `pr_number` as one COMMENT review, its body
    /// ([`ReviewPost::body`], within the forge's limit) marked as hunter's
    /// own ([`mark_own`]). Where each comment goes was decided
    /// by the caller from [`Self::pr_diff`]. A failed post may leave
    /// drafts of hunter's on the PR, which the caller deletes
    /// ([`Self::delete_own_drafts`]).
    fn post_review(
        &self,
        url: &str,
        pr_number: i64,
        post: &ReviewPost,
    ) -> anyhow::Result<PostedReview>;
    /// Delete every draft of hunter's the forge login has on PR
    /// `pr_number`: what a post that failed, or was stopped, before it
    /// published all of them left behind. A draft is the login's, so the
    /// operator's next review by hand would publish it under their name.
    /// Hunter's drafts are told from the operator's by [`OWN_MARK`], which
    /// every draft hunter creates carries. The number deleted; `Ok` means
    /// none of hunter's is left.
    fn delete_own_drafts(&self, url: &str, pr_number: i64) -> anyhow::Result<usize>;
    /// What a post of a review marked as step `step` ([`step_mark`]) left
    /// on PR `pr_number`, sent earlier by a call that failed: the forge
    /// may have acted on it all the same (a timeout, a 5xx after the
    /// write). The review is the forge login's whose body holds the step's
    /// mark on a line of its own; of `comments` (the issue, its inline
    /// comment's text as [`InlineComment::body`] gave it) those whose
    /// comment the login has on the PR, found as
    /// [`Self::inline_comment_refs`] finds them: on GitLab, whose notes
    /// are published one by one, by the issue's own mark, so a comment of
    /// another issue -- of this round or an earlier one, the same text or
    /// not -- is never taken for it. Only the login's count: anybody can
    /// copy hunter's text, mark included.
    fn find_review(
        &self,
        url: &str,
        pr_number: i64,
        step: &str,
        comments: &[(i64, &str)],
    ) -> anyhow::Result<FoundReview>;
    /// The forge's id of each inline comment `comments` (the issue it
    /// carries, its text as [`InlineComment::body`] gave it) of review
    /// `review_ref` ([`PostedReview::review_ref`]) of PR `pr_number`,
    /// found by its text; `comments` come in the order the post created
    /// them, which tells two of one text apart within one GitHub review,
    /// the forge numbering comments in creation order. GitLab's notes
    /// belong to no review, so each carries its issue's hidden mark, and
    /// only a note with the issue's own is its comment. Only comments the
    /// forge login wrote count: anybody can copy hunter's text, mark
    /// included, onto the PR. A comment missing from an `Ok` answer is not
    /// on the PR: the read lists every comment, so one it does not find
    /// was deleted.
    fn inline_comment_refs(
        &self,
        url: &str,
        pr_number: i64,
        review_ref: &str,
        comments: &[(i64, &str)],
    ) -> anyhow::Result<Vec<(i64, String)>>;
    /// The threads of PR `pr_number` whose first comment is one of
    /// `first_refs` (hunter's inline comments), each with its replies
    /// screened ([`screen_replies`]): the PR's author `pr_author` is
    /// trusted besides those who can push and `bots`. Threads others
    /// opened are not read. An `Ok` answer read every thread of the PR, so
    /// a thread of `first_refs` it does not hold is gone: one the PR has
    /// too many threads to read in full is an error.
    fn review_threads(
        &self,
        url: &str,
        pr_number: i64,
        first_refs: &[&str],
        pr_author: &str,
        bots: &ReviewBots,
    ) -> anyhow::Result<Vec<ReviewThread>>;
    /// Reply `body`, marked as hunter's own ([`mark_own`]), in the thread
    /// `reply_to` ([`ReviewThread::reply_to`]) names on PR `pr_number`,
    /// published at once: never into a review still pending.
    fn reply_to_thread(
        &self,
        url: &str,
        pr_number: i64,
        reply_to: &str,
        body: &str,
    ) -> anyhow::Result<()>;
    /// Resolve thread `thread_id` of PR `pr_number`, or unresolve it
    /// (`resolved` false).
    fn set_thread_resolved(
        &self,
        url: &str,
        pr_number: i64,
        thread_id: &str,
        resolved: bool,
    ) -> anyhow::Result<()>;
}

/// The local ref a PR's head is fetched into ([`Forge::pr_head_refspec`]).
pub fn pr_head_ref(pr_number: i64) -> String {
    format!("refs/hunter/pr/{pr_number}")
}

/// `https://<host>/<path>` of the repo at `url`, whatever form `url` is in.
fn repo_web_url(url: &str) -> String {
    url_host_path(url).map_or_else(
        || {
            url.trim_end_matches('/')
                .trim_end_matches(".git")
                .to_owned()
        },
        |(host, path)| {
            let path = path.trim_end_matches('/').trim_end_matches(".git");
            format!("https://{host}/{path}")
        },
    )
}

/// The `-R` slug of the GitHub repo at `url`.
fn github_slug(url: &str) -> anyhow::Result<String> {
    let (owner, repo) = github_owner_repo(url)
        .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
    Ok(format!("{owner}/{repo}"))
}

/// GitLab user id of `username` on the instance at `url`; `None` when no
/// such user exists. A name outside GitLab's username alphabet (letters,
/// digits, `_`, `-`, `.`) names no user: a group's path, `a/b`, is never
/// read as user `ab`.
fn gitlab_user_id(url: &str, username: &str) -> anyhow::Result<Option<i64>> {
    let wellformed = !username.is_empty()
        && username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if !wellformed {
        return Ok(None);
    }
    let api = format!("users?username={username}");
    let (rc, out) = gitlab_api(url, &api, None, &[]);
    if rc != 0 {
        anyhow::bail!("glab api {api} failed (rc={rc}): {out}");
    }
    let users: Vec<GlAuthor> = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("glab api {api}: unparseable users: {e}"))?;
    Ok(users.into_iter().find_map(|u| u.id))
}

/// The namespace owning GitLab project `project_id` on the instance at
/// `url`, by its full path: a user's username, or a group's path (`a/b`
/// for a subgroup). Empty when the project is gone, or hidden from
/// hunter's login (a 404): nobody GitLab names.
fn gitlab_project_owner(url: &str, project_id: i64) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct Namespace {
        #[serde(default, deserialize_with = "nullable")]
        full_path: String,
    }
    #[derive(Deserialize)]
    struct Project {
        #[serde(default)]
        namespace: Option<Namespace>,
    }
    let api = format!("projects/{project_id}");
    let (rc, out) = gitlab_api(url, &api, None, &[]);
    if rc != 0 {
        if out.contains("(HTTP 404)") {
            return Ok(String::new());
        }
        anyhow::bail!("glab api {api} failed (rc={rc}): {out}");
    }
    let project: Project = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("glab api {api}: unparseable project: {e}"))?;
    Ok(project.namespace.map_or_default(|n| n.full_path))
}

/// GitLab user id of the login `glab` is authenticated as on the
/// instance at `url`: hunter's own account.
fn gitlab_own_id(url: &str) -> anyhow::Result<i64> {
    let (rc, out) = gitlab_api(url, "user", None, &[]);
    if rc != 0 {
        anyhow::bail!("glab api user failed (rc={rc}): {out}");
    }
    let user: GlAuthor = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("glab api user: unparseable user: {e}"))?;
    user.id
        .ok_or_else(|| anyhow::anyhow!("glab api user: no user id in {}", out.trim()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// [`run_cmd`] in a working directory. Every `git`/`gh`/`glab` call here
/// runs inside a worktree, so the cwd is not optional for this module.
fn run_cmd_cwd(argv: &[&str], cwd: &Path, timeout_s: u64) -> (i32, String) {
    run_cmd_in(argv, Some(cwd), timeout_s)
}

/// Whether `host` is a GitHub instance: `github.com`, an Enterprise
/// Server install (which by convention carries a `github` label, as in
/// `github.corp.com`), or Enterprise Cloud's `<org>.ghe.com`.
///
/// Labels, not substrings, so `notgithub.com` does not qualify. This is
/// the single definition of "is GitHub" — `detect_forge` classifying a
/// host that the GitHub operations then reject is how every PR action
/// for an Enterprise repo used to fail with `cannot parse owner/repo`.
fn is_github_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    labels.contains(&"github") || labels.ends_with(&["ghe", "com"])
}

/// The `owner/repo` path of the GitHub repo at `url`, as REST paths name
/// it, with its host apart: `gh api` takes the host as `--hostname`, and a
/// path that also carried it would name a repo that does not exist. The
/// one parse of a GitHub URL's owner and repo ([`github_owner_repo`] adds
/// the host for `gh -R`).
fn github_repo_path(url: &str) -> Option<(&str, &str)> {
    // Every form `valid_repo_url` accepts, not just the two most common:
    // an `ssh://` remote that the add endpoint takes with a 201, or an
    // `http://` one stored before it refused them, must not then be
    // unparseable here, or the repo gets a PR that `sync_prs` can never
    // track.
    let (host, path) = url_host_path(url)?;
    if !is_github_host(host) {
        return None;
    }
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty()).then_some((host, path))
}

/// Parse owner/repo from a GitHub-style URL (HTTPS or SSH).
///
/// For an Enterprise host the owner half carries the host, giving
/// `gh`'s `[HOST/]OWNER/REPO` form — every consumer formats the pair
/// straight into `-R`, and without the host `gh` would talk to
/// github.com about a repo that only exists on the internal instance.
/// Never for an API path: that is [`github_repo_path`]'s.
fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let (host, path) = github_repo_path(url)?;
    let (owner, repo) = path.split_once('/')?;
    let owner = if host.eq_ignore_ascii_case("github.com") {
        owner.to_owned()
    } else {
        format!("{}/{owner}", host.to_ascii_lowercase())
    };
    Some((owner, repo.to_owned()))
}

/// Strip `<scheme>://` from `url`, comparing the scheme without regard
/// to case.
///
/// Schemes are case-insensitive per RFC 3986, and `valid_repo_url`
/// admits `HTTPS://…` because it lowercases before checking its
/// allow-list. Comparing case-sensitively here left the scheme inside
/// the authority, so parsing fell through to the scp-like branch and
/// read `HTTPS` as the host: no `github` label, so the repo was filed
/// as GitLab and every later PR operation ran `glab` against GitHub.
///
/// Only the scheme is folded. The path half carries the owner/repo
/// slug, which is case-sensitive.
fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let (head, rest) = url.split_at_checked(scheme.len())?;
    if head.eq_ignore_ascii_case(scheme) {
        rest.strip_prefix("://")
    } else {
        None
    }
}

/// Strip whichever of the schemes the write path accepts `url` carries.
fn strip_any_scheme(url: &str) -> Option<&str> {
    ["https", "http", "ssh"]
        .iter()
        .find_map(|s| strip_scheme(url, s))
}

/// Split a remote URL into (host, path), for every form the write path
/// accepts: `https://`, `http://`, `ssh://` and scp-like `git@host:path`.
/// Any `user@` and `:port` are stripped from the host.
fn url_host_path(url: &str) -> Option<(&str, &str)> {
    let host = url_host(url)?;
    let after_scheme = strip_any_scheme(url);
    let path = match after_scheme {
        // Past the authority, which is everything up to the first slash.
        Some(rest) => rest.split_once('/').map(|(_, p)| p)?,
        // Same bound as `url_host`: no slash before the colon, or it is
        // not the scp form.
        None => scp_split(url).map(|(_, p)| p)?,
    };
    (!path.is_empty()).then_some((host, path))
}

/// Extract (host, `project_path`) from a GitLab-style URL.
fn gitlab_host_path(url: &str) -> Option<(&str, &str)> {
    let (host, path) = url_host_path(url)?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    (!path.is_empty()).then_some((host, path))
}

/// Whether `a` and `b` name the same repository: the same host (any case,
/// any scheme, user or port) and path, a trailing `/` or `.git` aside. An
/// edit of a repo's URL that changes only its form (adding `.git`, `http`
/// to `https`, an `ssh` remote) names the same PRs; the path is compared
/// as given, the slug being case-sensitive here. A URL with no host
/// (a local path) compares as text, but for the same suffixes.
pub fn same_repo(a: &str, b: &str) -> bool {
    let trim = |p: &str| -> String { p.trim_end_matches('/').trim_end_matches(".git").to_owned() };
    let key = |url: &str| {
        url_host_path(url).map_or_else(
            || (String::new(), trim(url)),
            |(host, path)| (host.to_ascii_lowercase(), trim(path)),
        )
    };
    key(a) == key(b)
}

/// Build a `gh pr view --json` call and parse the `PrView` from JSON.
fn gh_pr_view(slug: &str, pr_number: i64, fields: &str) -> anyhow::Result<PrView> {
    let num = pr_number.to_string();
    let (rc, out) = run_cmd(
        &["gh", "pr", "view", &num, "-R", slug, "--json", fields],
        30,
    );
    if rc != 0 {
        anyhow::bail!("gh pr view failed (rc={rc}): {out}");
    }
    let v: serde_json::Value = serde_json::from_str(out.trim())?;
    let str_field = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
    Ok(PrView {
        state: match str_field("state").to_ascii_uppercase().as_str() {
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::Open,
        },
        mergeable: match str_field("mergeable").to_ascii_uppercase().as_str() {
            "MERGEABLE" => Mergeable::Mergeable,
            "CONFLICTING" => Mergeable::Conflicting,
            _ => Mergeable::Unknown,
        },
        review_decision: match str_field("reviewDecision").to_ascii_uppercase().as_str() {
            "APPROVED" => ReviewDecision::Approved,
            "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
            _ => ReviewDecision::None,
        },
        head_ref: str_field("headRefName").to_owned(),
        head_sha: str_field("headRefOid").to_owned(),
        updated_at: str_field("updatedAt").to_owned(),
        title: str_field("title").to_owned(),
        body: str_field("body").to_owned(),
        status_check_rollup: serde_json::from_value(
            v.get("statusCheckRollup").cloned().unwrap_or_default(),
        )
        .unwrap_or_default(),
        comments: serde_json::from_value(v.get("comments").cloned().unwrap_or_default())
            .unwrap_or_default(),
        reviews: serde_json::from_value(v.get("reviews").cloned().unwrap_or_default())
            .unwrap_or_default(),
        feedback_unread: false,
    })
}

/// Most pages of an MR's diff that `pr_diff` reads from GitLab: 1,000
/// files. The harvest prompt keeps only the first
/// [`crate::playbooks::PR_DIFF_CAP_CHARS`] characters of the diff, which
/// far fewer files already fill, so fetching past this buys the worker
/// nothing — and a server that ignored `page` would otherwise be asked
/// forever.
pub const MAX_DIFF_PAGES: usize = 10;

/// Most pages of 100 a read of a PR's review threads (an MR's
/// discussions, system notes among them), or of one thread's comments,
/// walks: 10,000. The read must be whole, since a thread of hunter's it
/// does not list is taken for deleted, so this is no budget but the
/// bound on a server that ignores the cursor or `page` and would
/// otherwise be asked forever; a real PR stays far below it.
pub const MAX_THREAD_PAGES: usize = 100;

/// Rebuild a unified diff from GitLab's per-file entries.
fn gitlab_unified_diff(files: &[GlFileDiff]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for f in files {
        let old = if f.new_file {
            "/dev/null".to_owned()
        } else {
            format!("a/{}", f.old_path)
        };
        let new = if f.deleted_file {
            "/dev/null".to_owned()
        } else {
            format!("b/{}", f.new_path)
        };
        let _ = write!(
            out,
            "diff --git a/{} b/{}\n--- {old}\n+++ {new}\n",
            f.old_path, f.new_path
        );
        // An empty `diff` under either flag is GitLab withholding the hunks,
        // not a file that changed nothing: say so, so the worker never
        // judges the PR by a diff that silently lost part of what it did.
        let omitted = if !f.diff.is_empty() {
            None
        } else if f.too_large {
            Some("file too large")
        } else if f.collapsed {
            Some("collapsed")
        } else {
            None
        };
        if let Some(why) = omitted {
            let _ = writeln!(out, "[diff omitted by GitLab: {why}]");
            continue;
        }
        out.push_str(&f.diff);
        if !f.diff.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// GitLab normalisation helpers (`forge.GitLabForge._norm_*`, `_split_notes`)
// ---------------------------------------------------------------------------

/// `locked` is GitLab's short-lived state for an MR being merged: it has
/// not ended yet, so it stays open until it reads `merged`.
fn gitlab_norm_state(raw: &str) -> PrState {
    match raw.to_ascii_lowercase().as_str() {
        "merged" => PrState::Merged,
        "closed" => PrState::Closed,
        _ => PrState::Open,
    }
}

fn gitlab_norm_mergeable(mr: &GlMergeRequest) -> Mergeable {
    let status = mr
        .detailed_merge_status
        .as_deref()
        .or(mr.merge_status.as_deref())
        .unwrap_or("")
        .to_ascii_lowercase();
    match status.as_str() {
        "mergeable" | "can_be_merged" | "ci_must_pass" | "ci_still_running" => Mergeable::Mergeable,
        s if s.contains("conflict") || s == "cannot_be_merged" => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    }
}

fn gitlab_norm_pipeline(mr: &GlMergeRequest) -> Vec<GhCheckRun> {
    let pipeline = mr.head_pipeline.as_ref().or(mr.pipeline.as_ref());
    let Some(pipeline) = pipeline else {
        return Vec::new();
    };
    let status = pipeline.status.to_ascii_lowercase();
    let conclusion = match status.as_str() {
        "success" => "SUCCESS",
        "failed" => "FAILURE",
        "canceled" => "CANCELLED",
        "skipped" => "SKIPPED",
        "running" => "IN_PROGRESS",
        "pending" | "created" => "PENDING",
        "manual" => "NEUTRAL",
        _ => {
            return vec![GhCheckRun {
                conclusion: Some(status.to_ascii_uppercase()),
                ..Default::default()
            }];
        }
    };
    vec![GhCheckRun {
        conclusion: Some(conclusion.to_owned()),
        ..Default::default()
    }]
}

/// GitLab notes normalized to comments, oldest first. A `DiffNote` keeps
/// where in the diff it was left; GitLab has no review bodies to speak of.
fn gitlab_split_notes(notes: &[GlNote]) -> Vec<GhComment> {
    notes
        .iter()
        .filter(|n| !n.system)
        .map(|n| GhComment {
            // Only GitHub screening asks by item; see `gitlab_feedback`.
            id: String::new(),
            created_at: if n.created_at.is_empty() {
                n.updated_at.clone()
            } else {
                n.created_at.clone()
            },
            body: n.body.clone(),
            author: n.author.as_ref().map(|a| GhAuthor {
                login: a.username.clone(),
            }),
            location: n.position.as_ref().map(GlPosition::location),
            voice: Voice::default(),
        })
        .collect()
}

/// Fetch GitLab MR + notes via `glab api`, returning typed structs. A notes
/// read that fails leaves the notes `None`, unless `strict_notes`
/// ([`Forge::view_pr_comments`], [`Forge::view_pr_engage`]), when it fails
/// the fetch.
fn gitlab_fetch_mr(
    url: &str,
    slug: &str,
    number: i64,
    strict_notes: bool,
) -> anyhow::Result<(GlMergeRequest, Option<Vec<GlNote>>)> {
    let enc = slug.replace('/', "%2F");
    let api_path = format!("projects/{enc}/merge_requests/{number}");
    let (rc, out) = gitlab_api(url, &api_path, None, &[]);
    if rc != 0 {
        anyhow::bail!("glab api {api_path} failed (rc={rc}): {out}");
    }
    let mr: GlMergeRequest = serde_json::from_str(out.trim())?;

    // Notes are a separate endpoint. GitLab caps `per_page` at 100 and
    // this reads a single page, so ask for the NEWEST one: the recent
    // reviewer feedback is exactly what `engage` and `sync_prs` act on,
    // and an oldest-first page silently dropped it on any MR with a long
    // thread. Flipped back below, since consumers want oldest-first.
    let notes_path = format!("projects/{enc}/merge_requests/{number}/notes?sort=desc&per_page=100");
    let (rc2, out2) = gitlab_api(url, &notes_path, None, &[]);
    let parsed = if rc2 == 0 {
        serde_json::from_str::<Vec<GlNote>>(out2.trim()).map_err(|e| e.to_string())
    } else {
        Err(format!("rc={rc2}: {out2}"))
    };
    let mut notes = match parsed {
        Ok(notes) => notes,
        Err(e) if strict_notes => anyhow::bail!("glab api {notes_path} failed ({e})"),
        Err(_) => return Ok((mr, None)),
    };
    notes.reverse();
    Ok((mr, Some(notes)))
}

/// Run `glab api <path>`, adding --hostname for self-hosted instances.
fn gitlab_api(
    url: &str,
    api_path: &str,
    method: Option<&str>,
    fields: &[(&str, &str)],
) -> (i32, String) {
    let mut args: Vec<String> = vec!["glab".into(), "api".into(), api_path.into()];
    if let Some(m) = method {
        args.extend(["--method".into(), m.into()]);
    }
    for (k, v) in fields {
        args.extend(["-f".into(), format!("{k}={v}")]);
    }
    if let Some((host, _)) = gitlab_host_path(url)
        && host != "gitlab.com"
    {
        args.extend(["--hostname".into(), host.into()]);
    }
    let refs: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
    run_cmd(&refs, 30)
}

/// `glab api --method <method> <path>` sending `body` (JSON) as the request
/// body: a draft note's position is nested, which `-f` fields cannot say.
fn gitlab_api_json(url: &str, api_path: &str, method: &str, body: &str) -> (i32, String) {
    let mut args = vec![
        "glab",
        "api",
        api_path,
        "--method",
        method,
        "--header",
        "Content-Type: application/json",
        "--input",
        "-",
    ];
    if let Some((host, _)) = gitlab_host_path(url)
        && host != "gitlab.com"
    {
        args.extend(["--hostname", host]);
    }
    run_cmd_input(&args, body, 30)
}

/// `text` as hunter sends it to GitLab, which runs a line that begins with
/// `/` and a command's name (`/merge`, `/approve`, `/close`, ...) as a
/// quick action under the poster's login, and strips it from the note: in
/// a note, a draft note once published, or an MR's description alike. The
/// text is the reviewer's or a worker's, quoting PR content, so every such
/// line gets a backslash before its slash: Markdown renders `\/` as `/`,
/// and no line begins with a command any more. GitLab drops every `\r`
/// before it looks, so they go first. Every text hunter posts to GitLab
/// goes through this, and so does the text a read-back matches what GitLab
/// lists against ([`Forge::inline_comment_refs`]).
fn gitlab_text(text: &str) -> String {
    let text = text.replace('\r', "");
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let command = line
            .strip_prefix('/')
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'));
        if command {
            out.push('\\');
        }
        out.push_str(line);
    }
    out
}

/// The step of issue `issue_id`'s inline note on GitLab ([`step_mark`]).
/// GitLab publishes a review's notes one by one, before its body, so a
/// send that failed may have left some on the MR, and its retry must tell
/// which issue each of hunter's notes there is ([`Forge::find_review`]).
/// Text alone cannot: two issues, of one round or of two, may read the
/// same. The issue is a row of one round, so a note carrying its mark was
/// published by a send of that round, for that issue, and no other.
fn issue_step(issue_id: i64) -> String {
    format!("comment of issue {issue_id}")
}

/// Issue `issue_id`'s inline comment `text` as hunter publishes it on
/// GitLab: marked with the issue's step ([`issue_step`]) and as hunter's
/// own ([`mark_own`]), escaped ([`gitlab_text`]). What a read-back matches
/// the MR's notes against ([`gitlab_published_refs`]).
fn gitlab_inline_note(issue_id: i64, text: &str) -> String {
    let marked = format!("{text}\n\n{}", step_mark(&issue_step(issue_id)));
    gitlab_text(&mark_own(&marked))
}

/// Create a draft note on MR `pr_number` (project `enc`, URL-encoded) from
/// `body`; the note's id.
fn gitlab_draft_note(
    url: &str,
    enc: &str,
    pr_number: i64,
    body: &serde_json::Value,
) -> anyhow::Result<i64> {
    #[derive(Deserialize)]
    struct Created {
        id: i64,
    }
    let api = format!("projects/{enc}/merge_requests/{pr_number}/draft_notes");
    let (rc, out) = gitlab_api_json(url, &api, "POST", &body.to_string());
    if rc != 0 {
        anyhow::bail!("glab api POST {api} failed (rc={rc}): {out}");
    }
    let created: Created = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("glab api POST {api}: unparseable draft note: {e}"))?;
    Ok(created.id)
}

/// The drafts of a GitLab review, published one by one: one per inline
/// comment, positioned on the diff `refs` names and marked with its
/// issue's step ([`gitlab_inline_note`]), then the body, each
/// marked as hunter's own ([`mark_own`]). Not `bulk_publish`: that
/// publishes every draft the operator's login has on the MR, a review they
/// started by hand included. Nothing is on the MR until the first publish:
/// a failure before it, or its refusal, says so ([`NothingPosted`]). A
/// failure part way leaves the inline notes published so far on the MR,
/// and the error says which ([`PartlyPosted`]); the body goes last, so the
/// post fails before it is out. Every draft it did not publish is left for
/// the caller to delete ([`Forge::delete_own_drafts`]), found by its mark
/// like any other a post left, a daemon stopped between creating and
/// publishing included.
fn gitlab_publish_review(
    url: &str,
    enc: &str,
    pr_number: i64,
    (post, refs): (&ReviewPost, &GlDiffRefs),
) -> anyhow::Result<PostedReview> {
    let mut drafts = Vec::with_capacity(post.comments.len() + 1);
    let mut inline = Vec::with_capacity(post.comments.len());
    for c in &post.comments {
        let mut position = serde_json::json!({
            "position_type": "text",
            "base_sha": refs.base_sha,
            "start_sha": refs.start_sha,
            "head_sha": refs.head_sha,
            "old_path": c.old_path,
            "new_path": c.path,
        });
        if let Some(line) = c.old_line {
            position["old_line"] = line.into();
        }
        if let Some(line) = c.new_line {
            position["new_line"] = line.into();
        }
        let note = serde_json::json!({
            "note": gitlab_inline_note(c.issue_id, &c.body),
            "position": position,
        });
        let id =
            gitlab_draft_note(url, enc, pr_number, &note).map_err(|e| e.context(NothingPosted))?;
        drafts.push(id);
        inline.push(c.issue_id);
    }
    let body = serde_json::json!({ "note": gitlab_text(&mark_own(&post.body())) });
    let body_id =
        gitlab_draft_note(url, enc, pr_number, &body).map_err(|e| e.context(NothingPosted))?;
    drafts.push(body_id);
    // The drafts are in `inline`'s order, the body last: the first
    // `published` of them are the inline notes out so far.
    for (published, id) in drafts.iter().enumerate() {
        let api = format!("projects/{enc}/merge_requests/{pr_number}/draft_notes/{id}/publish");
        let (rc, out) = gitlab_api(url, &api, Some("PUT"), &[]);
        if rc != 0 {
            let e = anyhow::anyhow!("glab api PUT {api} failed (rc={rc}): {out}");
            if published == 0 {
                return Err(if refused_outright(&out) {
                    e.context(NothingPosted)
                } else {
                    e
                });
            }
            inline.truncate(published);
            return Err(e.context(PartlyPosted { inline }));
        }
    }
    Ok(PostedReview {
        review_ref: body_id.to_string(),
        inline,
    })
}

/// One of the forge login's draft notes on an MR.
#[derive(Debug, Deserialize)]
struct GlDraftNote {
    id: i64,
    #[serde(default, deserialize_with = "nullable")]
    note: String,
}

/// Delete every draft note of hunter's on MR `pr_number` (project `enc`,
/// URL-encoded): see [`Forge::delete_own_drafts`]. GitLab lists the
/// login's drafts on the MR in one answer (the endpoint is not paginated),
/// the operator's own among them; only those whose last line is
/// [`OWN_MARK`] ([`is_own`]) are hunter's, and only those are deleted.
/// The number deleted; any failed call fails the whole, for a later call
/// to list what is left again.
fn gitlab_delete_own_drafts(url: &str, enc: &str, pr_number: i64) -> anyhow::Result<usize> {
    let api = format!("projects/{enc}/merge_requests/{pr_number}/draft_notes");
    let (rc, out) = gitlab_api(url, &api, None, &[]);
    if rc != 0 {
        anyhow::bail!("glab api {api} failed (rc={rc}): {out}");
    }
    let drafts: Vec<GlDraftNote> = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("glab api {api}: unparseable draft notes: {e}"))?;
    let mut deleted = 0;
    for draft in drafts.iter().filter(|d| is_own(&d.note)) {
        let api = format!("{api}/{}", draft.id);
        let (rc, out) = gitlab_api(url, &api, Some("DELETE"), &[]);
        if rc != 0 {
            anyhow::bail!("glab api DELETE {api} failed (rc={rc}): {out}");
        }
        deleted += 1;
    }
    Ok(deleted)
}

/// Whether a comment the forge lists reads `posted`: the text as sent, up
/// to line endings and surrounding whitespace, which a forge may change.
fn same_text(listed: &str, posted: &str) -> bool {
    let norm = |s: &str| s.replace("\r\n", "\n").trim().to_owned();
    norm(listed) == norm(posted)
}

/// Each of `comments` (issue, text), in the order one post created them,
/// matched to one of `listed` (the forge's creation-ordered id, the id to
/// record, text). A post creates its comments in the order given, each
/// the newest of its text when it is created, so of `listed` with one
/// text the newest, as many as `comments` has of it, are that post's, in
/// its order: two comments of one text each get their own, and a copy an
/// earlier post left is older than both. A comment with none left is left
/// out.
fn match_by_text<'a>(
    comments: &[(i64, &str)],
    listed: impl IntoIterator<Item = (i64, String, &'a str)>,
) -> Vec<(i64, String)> {
    let mut listed: Vec<(i64, String, &str)> = listed.into_iter().collect();
    listed.sort_by_key(|&(created, ..)| created);
    comments
        .iter()
        .enumerate()
        .filter_map(|(at, &(issue, text))| {
            let of_text: Vec<&String> = listed
                .iter()
                .filter(|(_, _, body)| same_text(body, text))
                .map(|(_, id, _)| id)
                .collect();
            let wanted = comments.iter().filter(|(_, t)| same_text(t, text)).count();
            let rank = comments[..at]
                .iter()
                .filter(|(_, t)| same_text(t, text))
                .count();
            let id = of_text.get(of_text.len().saturating_sub(wanted) + rank)?;
            Some((issue, (*id).clone()))
        })
        .collect()
}

/// The node id of each of `comments` (issue, text) among the inline
/// comments of GitHub review `review_id`, matched by text in the order the
/// review created them (their ids): see [`Forge::inline_comment_refs`].
/// Read from the PR's comment list, every page of it, filtered to the
/// review.
fn github_inline_refs(
    url: &str,
    pr_number: i64,
    review_id: &str,
    comments: &[(i64, &str)],
) -> anyhow::Result<Vec<(i64, String)>> {
    let review_id: i64 = review_id
        .parse()
        .map_err(|e| anyhow::anyhow!("GitHub review id {review_id:?}: {e}"))?;
    let (host, path) =
        url_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let api = format!("repos/{path}/pulls/{pr_number}/comments?per_page=100");
    let jq = format!(".[] | select(.pull_request_review_id == {review_id})");
    let mut argv = vec!["gh", "api", "--paginate", &api, "--jq", &jq];
    if !host.eq_ignore_ascii_case("github.com") {
        argv.extend(["--hostname", host]);
    }
    let (rc, out) = run_cmd(&argv, 60);
    if rc != 0 {
        anyhow::bail!("gh api {api} failed (rc={rc}): {out}");
    }
    let listed: Vec<GhReviewComment> = serde_json::Deserializer::from_str(&out)
        .into_iter::<GhReviewComment>()
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("gh api {api}: unparseable review comments: {e}"))?;
    Ok(match_by_text(
        comments,
        listed
            .iter()
            .map(|l| (l.id, l.node_id.clone(), l.body.as_str())),
    ))
}

/// See [`Forge::find_review`]: the PR's reviews, every page, read for the
/// forge login's (`gh api user`) whose body carries `step`'s mark; its
/// inline comments, which went up with it, by [`github_inline_refs`].
fn github_find_review(
    url: &str,
    pr_number: i64,
    step: &str,
    comments: &[(i64, &str)],
) -> anyhow::Result<FoundReview> {
    #[derive(Deserialize)]
    struct Listed {
        id: i64,
        #[serde(default)]
        body: String,
        #[serde(default)]
        login: String,
    }
    let (rc, login) = github_api_jq(url, "user", ".login")?;
    let login = login.trim();
    if rc != 0 || login.is_empty() {
        anyhow::bail!("gh api user failed (rc={rc}): {login}");
    }
    let (host, path) =
        url_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let api = format!("repos/{path}/pulls/{pr_number}/reviews?per_page=100");
    let jq = ".[] | {id, body, login: .user.login}";
    let mut argv = vec!["gh", "api", "--paginate", &api, "--jq", jq];
    if !host.eq_ignore_ascii_case("github.com") {
        argv.extend(["--hostname", host]);
    }
    let (rc, out) = run_cmd(&argv, 60);
    if rc != 0 {
        anyhow::bail!("gh api {api} failed (rc={rc}): {out}");
    }
    let listed: Vec<Listed> = serde_json::Deserializer::from_str(&out)
        .into_iter::<Listed>()
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("gh api {api}: unparseable reviews: {e}"))?;
    let own = normalize_login(login);
    let Some(found) = listed.iter().find(|r| {
        normalize_login(&r.login) == own && r.body.lines().any(|l| is_step_mark(l, step))
    }) else {
        return Ok(FoundReview::default());
    };
    let review_ref = found.id.to_string();
    let inline = github_inline_refs(url, pr_number, &review_ref, comments)?
        .into_iter()
        .map(|(issue, _)| issue)
        .collect();
    Ok(FoundReview {
        review_ref: Some(review_ref),
        inline,
    })
}

/// The current `owner/repo` of the GitHub repo at `url`. A renamed or
/// transferred repo answers its old path with a 301 on a GET, which gh
/// follows, but with a 307 on a POST, which it does not; and GraphQL names
/// a repository by its current name only. Asked on the repo's own host
/// ([`github_api_jq`]), by its host-free path ([`github_repo_path`]).
fn github_full_name(url: &str) -> anyhow::Result<String> {
    let (_, path) = github_repo_path(url)
        .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
    let (rc, full_name) = github_api_jq(url, &format!("repos/{path}"), ".full_name")?;
    let full_name = full_name.trim();
    if rc != 0 || full_name.is_empty() {
        anyhow::bail!("gh api repos/{path} failed (rc={rc}): {full_name}");
    }
    Ok(full_name.to_owned())
}

/// `gh api graphql` with `query`, string variables `vars` and integer ones
/// `ints`, on the host of the repo at `url`: the parsed response. One that
/// carries `errors` is an error, whatever else it holds.
fn github_graphql(
    url: &str,
    query: &str,
    vars: &[(&str, &str)],
    ints: &[(&str, i64)],
) -> anyhow::Result<serde_json::Value> {
    let (host, _) =
        url_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse host from {url}"))?;
    let mut args: Vec<String> = vec![
        "gh".into(),
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={query}"),
    ];
    // `-f` passes a value verbatim; `-F` would read a body starting with
    // `@` as a file to upload.
    for (k, v) in vars {
        args.extend(["-f".into(), format!("{k}={v}")]);
    }
    for (k, v) in ints {
        args.extend(["-F".into(), format!("{k}={v}")]);
    }
    if !host.eq_ignore_ascii_case("github.com") {
        args.extend(["--hostname".into(), host.into()]);
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (rc, out) = run_cmd(&refs, 60);
    if rc != 0 {
        anyhow::bail!("gh api graphql failed (rc={rc}): {out}");
    }
    let v: serde_json::Value = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("gh api graphql: unparseable response: {e}"))?;
    if let Some(errors) = v.get("errors").filter(|e| !e.is_null()) {
        anyhow::bail!("gh api graphql: {errors}");
    }
    Ok(v)
}

/// One page of a GraphQL connection: its `nodes` and where the next page
/// starts.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPage<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
    #[serde(default)]
    page_info: GqlPageInfo,
}

/// A GraphQL connection's `pageInfo`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageInfo {
    #[serde(default)]
    has_next_page: bool,
    #[serde(default)]
    end_cursor: Option<String>,
}

impl GqlPageInfo {
    /// The cursor to read the next page after, if there is one.
    fn next(&self) -> Option<&str> {
        self.end_cursor.as_deref().filter(|_| self.has_next_page)
    }
}

/// One of a PR's `reviewThreads`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThread {
    id: String,
    #[serde(default)]
    is_resolved: bool,
    comments: GqlPage<ThreadComment>,
}

/// Every review thread of GitHub PR `pr_number` in `owner/name`, each with
/// its first 100 comments, read in pages of 100. Threads come oldest
/// first: reading only the first page would lose the threads later rounds
/// open on a busy PR. A PR with more than [`MAX_THREAD_PAGES`] pages is
/// an error: a read that stopped short would take every thread past it
/// for gone ([`Forge::review_threads`]).
fn github_thread_pages(
    url: &str,
    (owner, name): (&str, &str),
    pr_number: i64,
) -> anyhow::Result<Vec<GqlThread>> {
    const QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $after: String) { \
                         repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
                         reviewThreads(first: 100, after: $after) { \
                         pageInfo { hasNextPage endCursor } nodes { id isResolved \
                         comments(first: 100) { pageInfo { hasNextPage endCursor } \
                         nodes { id fullDatabaseId author { login } body publishedAt } } \
                         } } } } }";
    let mut all = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_THREAD_PAGES {
        let mut vars = vec![("owner", owner), ("name", name)];
        if let Some(cursor) = after.as_deref() {
            vars.push(("after", cursor));
        }
        let v = github_graphql(url, QUERY, &vars, &[("number", pr_number)])?;
        let page = v
            .pointer("/data/repository/pullRequest/reviewThreads")
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("gh api graphql: PR #{pr_number} has no review threads")
            })?;
        let page: GqlPage<GqlThread> = serde_json::from_value(page)
            .map_err(|e| anyhow::anyhow!("gh api graphql: unparseable review threads: {e}"))?;
        after = page.page_info.next().map(str::to_owned);
        all.extend(page.nodes);
        if after.is_none() {
            return Ok(all);
        }
    }
    anyhow::bail!(
        "PR #{pr_number} has more than {} review threads; not reading them",
        100 * MAX_THREAD_PAGES
    )
}

/// The rest of thread `thread`'s comments past its first page, up to
/// [`MAX_THREAD_PAGES`] more pages: the newest replies are the last ones.
fn github_more_comments(url: &str, thread: &mut GqlThread) -> anyhow::Result<()> {
    const QUERY: &str = "query($thread: ID!, $after: String!) { node(id: $thread) { \
                         ... on PullRequestReviewThread { comments(first: 100, after: $after) { \
                         pageInfo { hasNextPage endCursor } \
                         nodes { id author { login } body publishedAt } } } } }";
    let mut after = thread.comments.page_info.next().map(str::to_owned);
    for _ in 0..MAX_THREAD_PAGES {
        let Some(cursor) = after.take() else { break };
        let v = github_graphql(
            url,
            QUERY,
            &[("thread", &thread.id), ("after", &cursor)],
            &[],
        )?;
        let page = v.pointer("/data/node/comments").cloned().ok_or_else(|| {
            anyhow::anyhow!("gh api graphql: thread {} has no comments", thread.id)
        })?;
        let page: GqlPage<ThreadComment> = serde_json::from_value(page)
            .map_err(|e| anyhow::anyhow!("gh api graphql: unparseable thread comments: {e}"))?;
        after = page.page_info.next().map(str::to_owned);
        thread.comments.nodes.extend(page.nodes);
    }
    Ok(())
}

/// The threads of GitHub PR `pr_number` that one of `first_refs` opened:
/// see [`Forge::review_threads`]. Read page by page
/// ([`github_thread_pages`], [`github_more_comments`]).
fn github_review_threads(
    url: &str,
    pr_number: i64,
    first_refs: &[&str],
    pr_author: &str,
    bots: &ReviewBots,
) -> anyhow::Result<Vec<ReviewThread>> {
    let full_name = github_full_name(url)?;
    let (owner, name) = full_name
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("unexpected repo name {full_name:?}"))?;
    let threads = github_thread_pages(url, (owner, name), pr_number)?;
    let mut ours: Vec<GqlThread> = threads
        .into_iter()
        .filter(|t| {
            t.comments
                .nodes
                .first()
                .is_some_and(|c| first_refs.contains(&c.id.as_str()))
        })
        .collect();
    for t in &mut ours {
        github_more_comments(url, t)?;
    }
    let (host, repo) = github_cache_scope(url);
    let bot_ids: Vec<&str> = ours
        .iter()
        .flat_map(|t| t.comments.nodes.iter().skip(1))
        .filter(|c| !c.id.is_empty() && c.author.as_ref().is_some_and(|a| bots.contains(&a.login)))
        .map(|c| c.id.as_str())
        .collect();
    let bot_authored = if bot_ids.is_empty() {
        HashSet::new()
    } else {
        github_bot_authored(url, &host, &bot_ids)?
    };
    ours.into_iter()
        .map(|t| {
            let mut comments = t.comments.nodes.into_iter();
            let first = comments.next().unwrap_or_default();
            let poster = first.author.map_or_default(|a| a.login);
            let (replies, hidden) = screen_replies(
                comments.collect(),
                ThreadVoices {
                    pr_author,
                    poster: &poster,
                },
                bots,
                |id, _| Ok(bot_authored.contains(id)),
                |login| {
                    cached_lookup(
                        format!("github-push:{repo}:{}", normalize_login(login)),
                        || github_can_push(url, login),
                    )
                },
            )?;
            Ok(ReviewThread {
                first_ref: first.id,
                thread_id: t.id,
                reply_to: first.full_database_id.unwrap_or_default(),
                resolved: t.is_resolved,
                replies,
                hidden,
            })
        })
        .collect()
}

/// One of an MR's `discussions`.
#[derive(Debug, Default, Deserialize)]
struct GlDiscussion {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    notes: Vec<GlNote>,
}

/// Every discussion of MR `pr_number` (project `enc`, URL-encoded), read
/// in pages of 100. An MR with more than [`MAX_THREAD_PAGES`] full pages
/// is an error: a read that stopped short would take every discussion
/// past it for gone ([`Forge::review_threads`],
/// [`Forge::inline_comment_refs`]).
fn gitlab_discussions(url: &str, enc: &str, pr_number: i64) -> anyhow::Result<Vec<GlDiscussion>> {
    const PER_PAGE: usize = 100;
    let mut all = Vec::new();
    for page in 1..=MAX_THREAD_PAGES {
        let api = format!(
            "projects/{enc}/merge_requests/{pr_number}/discussions?per_page={PER_PAGE}&page={page}"
        );
        let (rc, out) = gitlab_api(url, &api, None, &[]);
        if rc != 0 {
            anyhow::bail!("glab api {api} failed (rc={rc}): {out}");
        }
        let batch: Vec<GlDiscussion> = serde_json::from_str(out.trim())
            .map_err(|e| anyhow::anyhow!("glab api {api}: unparseable discussions: {e}"))?;
        let last = batch.len() < PER_PAGE;
        all.extend(batch);
        if last {
            return Ok(all);
        }
    }
    anyhow::bail!(
        "MR !{pr_number} has at least {} discussions; not reading them",
        PER_PAGE * MAX_THREAD_PAGES
    )
}

/// The id of the note each of `comments` (issue, its comment's text as the
/// round wrote it) became once published: the first note, with a position
/// on the diff and written by hunter's own account (user `own_id`), of a
/// discussion reading as the issue's note does ([`gitlab_inline_note`]),
/// the issue's own mark included -- so the note of another issue never
/// matches, however alike their texts, be it one of the same round or one
/// an earlier round posted -- the newest such ([`match_by_text`]; see
/// [`Forge::inline_comment_refs`]). A thread is found by its first note's
/// id, which publishing gives a draft. Somebody else's copy of the note,
/// mark included, is never hunter's thread, however new.
fn gitlab_published_refs(
    discussions: &[GlDiscussion],
    own_id: i64,
    comments: &[(i64, &str)],
) -> Vec<(i64, String)> {
    let notes: Vec<(i64, String)> = comments
        .iter()
        .map(|&(issue, text)| (issue, gitlab_inline_note(issue, text)))
        .collect();
    let notes: Vec<(i64, &str)> = notes.iter().map(|(i, t)| (*i, t.as_str())).collect();
    let firsts = discussions
        .iter()
        .filter_map(|d| d.notes.first())
        .filter(|n| n.position.is_some())
        .filter(|n| n.author.as_ref().and_then(|a| a.id) == Some(own_id))
        .filter_map(|n| Some((n.id?, n.body.as_str())));
    match_by_text(&notes, firsts.map(|(id, body)| (id, id.to_string(), body)))
}

/// See [`Forge::find_review`]: the MR's discussions read for a note of
/// hunter's own account (user `own_id`) off the diff whose body carries
/// `step`'s mark -- a review's body, published last -- and for the inline
/// notes of `comments`, published one by one before it, found as
/// [`gitlab_published_refs`] finds them.
fn gitlab_find_review(
    discussions: &[GlDiscussion],
    own_id: i64,
    step: &str,
    comments: &[(i64, &str)],
) -> FoundReview {
    let review_ref = discussions
        .iter()
        .flat_map(|d| &d.notes)
        .filter(|n| n.position.is_none())
        .filter(|n| n.author.as_ref().and_then(|a| a.id) == Some(own_id))
        .find(|n| n.body.lines().any(|l| is_step_mark(l, step)))
        .and_then(|n| n.id)
        .map(|id| id.to_string());
    let inline = gitlab_published_refs(discussions, own_id, comments)
        .into_iter()
        .map(|(issue, _)| issue)
        .collect();
    FoundReview { review_ref, inline }
}

/// The threads of GitLab MR `pr_number` that one of `first_refs` opened:
/// see [`Forge::review_threads`]. Membership is looked up by user id, as
/// in [`gitlab_feedback`], and a configured bot needs no confirmation.
fn gitlab_review_threads(
    url: &str,
    pr_number: i64,
    first_refs: &[&str],
    pr_author: &str,
    bots: &ReviewBots,
) -> anyhow::Result<Vec<ReviewThread>> {
    let (host, project) =
        gitlab_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
    let enc = project.replace('/', "%2F");
    let host = host.to_ascii_lowercase();
    gitlab_discussions(url, &enc, pr_number)?
        .into_iter()
        .filter_map(|d| {
            let first = d.notes.first()?.id?.to_string();
            first_refs.contains(&first.as_str()).then_some((first, d))
        })
        .map(|(first_ref, d)| {
            let resolved = d.notes.first().is_some_and(|n| n.resolved);
            let poster = d
                .notes
                .first()
                .and_then(|n| n.author.as_ref())
                .map_or_default(|a| a.username.clone());
            let ids: HashMap<String, i64> = d
                .notes
                .iter()
                .filter_map(|n| n.author.as_ref())
                .filter_map(|a| Some((normalize_login(&a.username), a.id?)))
                .collect();
            let replies = d
                .notes
                .iter()
                .skip(1)
                .filter(|n| !n.system)
                .map(|n| ThreadComment {
                    id: n.id.map_or_default(|id| id.to_string()),
                    full_database_id: None,
                    author: n.author.as_ref().map(|a| GhAuthor {
                        login: a.username.clone(),
                    }),
                    body: n.body.clone(),
                    published_at: Some(n.created_at.clone()),
                })
                .collect();
            let (replies, hidden) = screen_replies(
                replies,
                ThreadVoices {
                    pr_author,
                    poster: &poster,
                },
                bots,
                |_, _| Ok(true),
                |login| {
                    let Some(&id) = ids.get(&normalize_login(login)) else {
                        anyhow::bail!("GitLab note by {login:?} carries no author id");
                    };
                    cached_lookup(format!("gitlab-push:{host}/{project}:{id}"), || {
                        gitlab_can_push(url, project, id)
                    })
                },
            )?;
            Ok(ReviewThread {
                first_ref,
                reply_to: d.id.clone(),
                thread_id: d.id,
                resolved,
                replies,
                hidden,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// GitHub (via `gh` CLI) — `forge.GitHubForge`
// ---------------------------------------------------------------------------

pub struct GitHubForge;
pub struct GitLabForge;

impl Forge for GitHubForge {
    fn owner_repo(&self, url: &str) -> Option<(String, String)> {
        github_owner_repo(url)
    }

    fn parse_pr_url(&self, url: &str) -> Option<(String, i64)> {
        // https://<github host>/owner/repo/pull/123
        let (host, path) = url_host_path(url)?;
        if strip_scheme(url, "https").is_none() || !is_github_host(host) {
            return None;
        }
        let (slug, num_part) = path.rsplit_once("/pull/")?;
        let num: i64 = num_part.split(&['/', '?', '#'][..]).next()?.parse().ok()?;
        // Same `[HOST/]OWNER/REPO` slug the owner_repo pair produces,
        // since both end up in `gh -R`.
        let slug = if host.eq_ignore_ascii_case("github.com") {
            slug.to_owned()
        } else {
            format!("{}/{slug}", host.to_ascii_lowercase())
        };
        Some((slug, num))
    }

    fn create_pr(
        &self,
        repo_path: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        let (rc, out) = run_cmd_cwd(
            &[
                "gh",
                "pr",
                "create",
                "--draft",
                "--head",
                head,
                "--base",
                base,
                "--title",
                if title.is_empty() { head } else { title },
                "--body",
                body,
            ],
            repo_path,
            300,
        );
        if rc != 0 {
            anyhow::bail!("gh pr create failed (rc={rc}): {out}");
        }
        // The PR URL is the last line `parse_pr_url` accepts: `out` carries
        // stderr too, so the last line may be a warning.
        match out
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| self.parse_pr_url(l).is_some())
        {
            Some(url) => Ok(url.to_owned()),
            None => anyhow::bail!("gh pr create succeeded but printed no PR URL: {out}"),
        }
    }

    fn view_pr_sync(&self, url: &str, pr_number: i64, bots: &ReviewBots) -> anyhow::Result<PrView> {
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        let mut pr = gh_pr_view(
            &slug,
            pr_number,
            "state,mergedAt,mergeable,reviewDecision,statusCheckRollup,comments,reviews,updatedAt,headRefName,headRefOid",
        )?;
        github_screen(url, &mut pr, bots)?;
        Ok(pr)
    }

    fn view_pr_engage(
        &self,
        url: &str,
        pr_number: i64,
        bots: &ReviewBots,
    ) -> anyhow::Result<PrView> {
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        let mut pr = gh_pr_view(
            &slug,
            pr_number,
            "state,title,body,comments,reviews,statusCheckRollup,headRefName,headRefOid",
        )?;
        // Engage acts on what reviewers said about specific lines, and
        // `gh pr view` returns only the conversation and review summaries.
        pr.comments.extend(github_review_comments(url, pr_number)?);
        github_screen(url, &mut pr, bots)?;
        Ok(pr)
    }

    fn pr_diff(&self, url: &str, pr_number: i64) -> anyhow::Result<String> {
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(
            &["gh", "pr", "diff", &num, "-R", &slug, "--color", "never"],
            60,
        );
        if rc != 0 {
            anyhow::bail!("gh pr diff failed (rc={rc}): {out}");
        }
        Ok(out)
    }

    fn comment_pr(&self, url: &str, pr_number: i64, body: &str) -> anyhow::Result<()> {
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(
            &[
                "gh",
                "pr",
                "comment",
                &num,
                "-R",
                &slug,
                "--body",
                &mark_own(body),
            ],
            60,
        );
        if rc != 0 {
            anyhow::bail!("gh pr comment failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn close_pr(&self, url: &str, pr_number: i64, comment: &str) -> anyhow::Result<()> {
        // Post the withdrawal reason as a comment first, then close. Only
        // close once it lands — a lost reason must not become a silent close.
        if !comment.is_empty() {
            self.comment_pr(url, pr_number, comment)?;
        }
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(&["gh", "pr", "close", &num, "-R", &slug], 60);
        if rc != 0 {
            anyhow::bail!("gh pr close failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn push(&self, repo_path: &Path, url: &str, branch: &str) -> anyhow::Result<()> {
        let refspec = format!("HEAD:{branch}");
        let (rc, out) = Unheld::Push {
            force: Force::Always,
            url,
            refspec: &refspec,
        }
        .run(repo_path, 120);
        if rc != 0 {
            anyhow::bail!("git push failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn view_pr_meta(&self, url: &str, pr_number: i64) -> anyhow::Result<PrMeta> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct View {
            #[serde(default)]
            state: String,
            #[serde(default)]
            head_ref_oid: String,
            #[serde(default)]
            head_ref_name: String,
            #[serde(default)]
            base_ref_name: String,
            #[serde(default)]
            author: Option<GhAuthor>,
            #[serde(default)]
            title: String,
            #[serde(default)]
            body: String,
            #[serde(default)]
            is_cross_repository: bool,
            #[serde(default)]
            head_repository_owner: Option<GhAuthor>,
        }
        let slug = github_slug(url)?;
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(
            &[
                "gh",
                "pr",
                "view",
                &num,
                "-R",
                &slug,
                "--json",
                "state,headRefOid,headRefName,baseRefName,author,title,body,isCrossRepository,\
                 headRepositoryOwner",
            ],
            30,
        );
        if rc != 0 {
            anyhow::bail!("gh pr view failed (rc={rc}): {out}");
        }
        let v: View = serde_json::from_str(out.trim())
            .map_err(|e| anyhow::anyhow!("gh pr view: unparseable PR: {e}"))?;
        Ok(PrMeta {
            state: match v.state.to_ascii_uppercase().as_str() {
                "MERGED" => PrState::Merged,
                "CLOSED" => PrState::Closed,
                _ => PrState::Open,
            },
            head_sha: v.head_ref_oid,
            head_ref: v.head_ref_name,
            base_ref: v.base_ref_name,
            author: v.author.map_or_default(|a| a.login),
            title: v.title,
            body: v.body,
            cross_repo: v.is_cross_repository,
            head_owner: v
                .head_repository_owner
                .filter(|_| v.is_cross_repository)
                .map_or_default(|o| o.login),
        })
    }

    fn list_open_prs(&self, url: &str) -> anyhow::Result<Vec<OpenPr>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Listed {
            number: i64,
            #[serde(default)]
            title: String,
            #[serde(default)]
            author: Option<GhAuthor>,
            #[serde(default)]
            is_draft: bool,
            #[serde(default)]
            is_cross_repository: bool,
            #[serde(default)]
            url: String,
        }
        let slug = github_slug(url)?;
        let (rc, out) = run_cmd(
            &[
                "gh",
                "pr",
                "list",
                "-R",
                &slug,
                "--state",
                "open",
                "--json",
                "number,title,author,isDraft,isCrossRepository,url",
                "-L",
                "100",
            ],
            30,
        );
        if rc != 0 {
            anyhow::bail!("gh pr list failed (rc={rc}): {out}");
        }
        let listed: Vec<Listed> = serde_json::from_str(out.trim())
            .map_err(|e| anyhow::anyhow!("gh pr list: unparseable list: {e}"))?;
        Ok(listed
            .into_iter()
            .map(|p| OpenPr {
                number: p.number,
                title: p.title,
                author: p.author.map_or_default(|a| a.login),
                is_draft: p.is_draft,
                cross_repo: p.is_cross_repository,
                url: p.url,
            })
            .collect())
    }

    fn can_push(&self, url: &str, login: &str) -> anyhow::Result<bool> {
        let repo = url_host_path(url).map_or_else(
            || url.to_owned(),
            |(host, path)| format!("{}/{path}", host.to_ascii_lowercase()),
        );
        cached_lookup(
            format!("github-push:{repo}:{}", normalize_login(login)),
            || github_can_push(url, login),
        )
    }

    fn pr_web_url(&self, url: &str, pr_number: i64) -> String {
        format!("{}/pull/{pr_number}", repo_web_url(url))
    }

    fn pr_head_refspec(&self, pr_number: i64) -> String {
        format!("+refs/pull/{pr_number}/head:{}", pr_head_ref(pr_number))
    }

    fn post_review(
        &self,
        url: &str,
        pr_number: i64,
        post: &ReviewPost,
    ) -> anyhow::Result<PostedReview> {
        #[derive(Deserialize)]
        struct Created {
            id: i64,
        }
        // Nothing is on the PR until the POST: a failure before it, or the
        // POST's refusal, puts none of the review there.
        let before = |e: anyhow::Error| e.context(NothingPosted);
        let (host, _) = url_host_path(url)
            .ok_or_else(|| before(anyhow::anyhow!("cannot parse owner/repo from {url}")))?;
        let full_name = github_full_name(url).map_err(before)?;
        let api = format!("repos/{full_name}/pulls/{pr_number}/reviews");
        let comments: Vec<serde_json::Value> = post
            .comments
            .iter()
            .map(|c| {
                serde_json::json!({
                    "path": c.path,
                    "line": c.line,
                    "side": c.side.as_str(),
                    "body": c.body,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "commit_id": post.commit_id,
            "event": "COMMENT",
            "body": mark_own(&post.body()),
            "comments": comments,
        });
        let mut argv = vec!["gh", "api", "-X", "POST", &api, "--input", "-"];
        if !host.eq_ignore_ascii_case("github.com") {
            argv.extend(["--hostname", host]);
        }
        let (rc, out) = run_cmd_input(&argv, &payload.to_string(), 60);
        if rc != 0 {
            let e = anyhow::anyhow!("gh api POST {api} failed (rc={rc}): {out}");
            return Err(if refused_outright(&out) {
                e.context(NothingPosted)
            } else {
                e
            });
        }
        let created: Created = serde_json::from_str(out.trim())
            .map_err(|e| anyhow::anyhow!("gh api POST {api}: unparseable review: {e}"))?;
        Ok(PostedReview {
            review_ref: created.id.to_string(),
            inline: post.comments.iter().map(|c| c.issue_id).collect(),
        })
    }

    fn delete_own_drafts(&self, _url: &str, _pr_number: i64) -> anyhow::Result<usize> {
        // A review is created and submitted in the one call above, its
        // comments with it: hunter never leaves a draft on GitHub.
        Ok(0)
    }

    fn find_review(
        &self,
        url: &str,
        pr_number: i64,
        step: &str,
        comments: &[(i64, &str)],
    ) -> anyhow::Result<FoundReview> {
        github_find_review(url, pr_number, step, comments)
    }

    fn inline_comment_refs(
        &self,
        url: &str,
        pr_number: i64,
        review_ref: &str,
        comments: &[(i64, &str)],
    ) -> anyhow::Result<Vec<(i64, String)>> {
        github_inline_refs(url, pr_number, review_ref, comments)
    }

    fn review_threads(
        &self,
        url: &str,
        pr_number: i64,
        first_refs: &[&str],
        pr_author: &str,
        bots: &ReviewBots,
    ) -> anyhow::Result<Vec<ReviewThread>> {
        github_review_threads(url, pr_number, first_refs, pr_author, bots)
    }

    /// By REST, under the thread's first comment: a reply published at
    /// once. GraphQL's `addPullRequestReviewThreadReply` would put it in
    /// the login's pending review whenever the operator has one open on
    /// the PR, unseen until they submit it while hunter takes it as said
    /// and resolves the thread. With such a review open GitHub refuses
    /// this call instead (one pending review per login and PR), so the
    /// post fails, and is retried, rather than answering unseen.
    fn reply_to_thread(
        &self,
        url: &str,
        pr_number: i64,
        reply_to: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        if reply_to.is_empty() || !reply_to.bytes().all(|b| b.is_ascii_digit()) {
            anyhow::bail!(
                "the thread's first comment has no database id to reply to: {reply_to:?}"
            );
        }
        let (host, _) = url_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let full_name = github_full_name(url)?;
        let api = format!("repos/{full_name}/pulls/{pr_number}/comments/{reply_to}/replies");
        let field = format!("body={}", mark_own(body));
        let mut argv = vec!["gh", "api", "-X", "POST", &api, "-f", &field];
        if !host.eq_ignore_ascii_case("github.com") {
            argv.extend(["--hostname", host]);
        }
        let (rc, out) = run_cmd(&argv, 60);
        if rc != 0 {
            anyhow::bail!("gh api POST {api} failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn set_thread_resolved(
        &self,
        url: &str,
        _pr_number: i64,
        thread_id: &str,
        resolved: bool,
    ) -> anyhow::Result<()> {
        const RESOLVE: &str = "mutation($thread: ID!) { resolveReviewThread(input: \
                               {threadId: $thread}) { thread { isResolved } } }";
        const UNRESOLVE: &str = "mutation($thread: ID!) { unresolveReviewThread(input: \
                                 {threadId: $thread}) { thread { isResolved } } }";
        let query = if resolved { RESOLVE } else { UNRESOLVE };
        github_graphql(url, query, &[("thread", thread_id)], &[])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitLab (via `glab` CLI + REST API) — `forge.GitLabForge`
// ---------------------------------------------------------------------------

impl Forge for GitLabForge {
    fn owner_repo(&self, url: &str) -> Option<(String, String)> {
        let (_, path) = gitlab_host_path(url)?;
        // Split at last '/' — GitLab paths may be multi-level (group/sub/repo).
        let (prefix, last) = path.rsplit_once('/')?;
        if prefix.is_empty() || last.is_empty() {
            return None;
        }
        Some((prefix.to_owned(), last.to_owned()))
    }

    fn parse_pr_url(&self, url: &str) -> Option<(String, i64)> {
        // https://gitlab.com/group/subgroup/repo/-/merge_requests/42
        let after_host = strip_scheme(url, "https")
            .and_then(|s| s.split_once('/'))?
            .1;
        let (slug, num_part) = after_host.rsplit_once("/-/merge_requests/")?;
        let num: i64 = num_part.split(&['/', '?', '#'][..]).next()?.parse().ok()?;
        Some((slug.to_owned(), num))
    }

    fn create_pr(
        &self,
        repo_path: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        // Derive -R flag from the git remote in cwd (glab infers host).
        let title_arg = if title.is_empty() { head } else { title };
        let (rc, out) = run_cmd_cwd(
            &[
                "glab",
                "mr",
                "create",
                "--source-branch",
                head,
                "--target-branch",
                base,
                "--draft",
                "--title",
                title_arg,
                "--description",
                &gitlab_text(body),
                "--yes",
            ],
            repo_path,
            300,
        );
        if rc != 0 {
            // GitLab's 409 names the open MR only as `!N`, and glab wraps
            // its error box at a fixed width, which can split the phrase
            // across lines. Look the MR up so the caller can adopt it, as
            // it adopts the PR URL `gh` prints for the same conflict.
            let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
            if flat.contains("already exists") {
                let (lrc, listed) = run_cmd_cwd(
                    &[
                        "glab",
                        "mr",
                        "list",
                        "--source-branch",
                        head,
                        "--target-branch",
                        base,
                        "--output",
                        "json",
                        "--jq",
                        ".[].web_url",
                    ],
                    repo_path,
                    60,
                );
                if lrc == 0
                    && let Some(url) = listed
                        .lines()
                        .map(str::trim)
                        .find(|l| l.contains("/-/merge_requests/"))
                {
                    anyhow::bail!(
                        "glab mr create failed (rc={rc}): merge request already exists: {url}\n{out}"
                    );
                }
            }
            anyhow::bail!("glab mr create failed (rc={rc}): {out}");
        }
        // glab prints the MR URL and keeps talking; search for it.
        match out
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| self.parse_pr_url(l).is_some())
        {
            Some(url) => Ok(url.to_owned()),
            None => anyhow::bail!("glab mr create succeeded but printed no MR URL: {out}"),
        }
    }

    fn view_pr_sync(&self, url: &str, pr_number: i64, bots: &ReviewBots) -> anyhow::Result<PrView> {
        gitlab_view_sync(url, pr_number, bots, false)
    }

    fn view_pr_comments(
        &self,
        url: &str,
        pr_number: i64,
        bots: &ReviewBots,
    ) -> anyhow::Result<PrView> {
        gitlab_view_sync(url, pr_number, bots, true)
    }

    fn view_pr_engage(
        &self,
        url: &str,
        pr_number: i64,
        bots: &ReviewBots,
    ) -> anyhow::Result<PrView> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        // Engage acts on the feedback: notes that could not be read are not
        // an MR without feedback, so the view fails instead.
        let (mr, notes) = gitlab_fetch_mr(url, path, pr_number, true)?;
        let comments = gitlab_feedback(url, path, notes.as_deref().unwrap_or_default(), bots)?;
        let state = gitlab_norm_state(&mr.state);
        let mergeable = gitlab_norm_mergeable(&mr);
        let status_check_rollup = gitlab_norm_pipeline(&mr);
        Ok(PrView {
            state,
            mergeable,
            review_decision: ReviewDecision::None,
            head_ref: mr.source_branch,
            head_sha: mr.sha,
            updated_at: mr.updated_at,
            title: mr.title,
            body: mr.description,
            status_check_rollup,
            comments,
            reviews: Vec::new(),
            feedback_unread: notes.is_none(),
        })
    }

    fn pr_diff(&self, url: &str, pr_number: i64) -> anyhow::Result<String> {
        // The endpoint is paginated and GitLab caps `per_page` at 100, so
        // one request silently truncates any MR touching more files than
        // that; the harvest would then judge a closed PR by part of what
        // it proposed. Walk the pages until one comes back short.
        const PER_PAGE: usize = 100;
        use std::fmt::Write as _;
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        let mut files: Vec<GlFileDiff> = Vec::new();
        let mut complete = false;
        for page in 1..=MAX_DIFF_PAGES {
            let api_path = format!(
                "projects/{enc}/merge_requests/{pr_number}/diffs?per_page={PER_PAGE}&page={page}"
            );
            let (rc, out) = gitlab_api(url, &api_path, None, &[]);
            if rc != 0 {
                anyhow::bail!("glab api {api_path} failed (rc={rc}): {out}");
            }
            let batch: Vec<GlFileDiff> = serde_json::from_str(out.trim())?;
            complete = batch.len() < PER_PAGE;
            files.extend(batch);
            if complete {
                break;
            }
        }
        let mut diff = gitlab_unified_diff(&files);
        if !complete {
            // Stopped at the cap with pages possibly left: say so, or the
            // worker reads part of the MR as all of it.
            let shown = PER_PAGE * MAX_DIFF_PAGES;
            let _ = writeln!(
                diff,
                "[diff truncated: MR has more than {shown} changed files; only the first {shown} are shown]"
            );
        }
        Ok(diff)
    }
    fn comment_pr(&self, url: &str, pr_number: i64, body: &str) -> anyhow::Result<()> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        let api_path = format!("projects/{enc}/merge_requests/{pr_number}/notes");
        let body = gitlab_text(&mark_own(body));
        let (rc, out) = gitlab_api(url, &api_path, Some("POST"), &[("body", &body)]);
        if rc != 0 {
            anyhow::bail!("glab api POST notes failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn close_pr(&self, url: &str, pr_number: i64, comment: &str) -> anyhow::Result<()> {
        // Close only after the withdrawal reason has landed.
        if !comment.is_empty() {
            self.comment_pr(url, pr_number, comment)?;
        }
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let repo_flag = if let Some((host, _)) = gitlab_host_path(url)
            && host != "gitlab.com"
        {
            format!("https://{host}/{path}")
        } else {
            path.to_owned()
        };
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(&["glab", "mr", "close", &num, "-R", &repo_flag], 60);
        if rc != 0 {
            anyhow::bail!("glab mr close failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn push(&self, repo_path: &Path, url: &str, branch: &str) -> anyhow::Result<()> {
        let refspec = format!("HEAD:{branch}");
        let (rc, out) = Unheld::Push {
            force: Force::Always,
            url,
            refspec: &refspec,
        }
        .run(repo_path, 120);
        if rc != 0 {
            anyhow::bail!("git push failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn view_pr_meta(&self, url: &str, pr_number: i64) -> anyhow::Result<PrMeta> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let api = format!(
            "projects/{}/merge_requests/{pr_number}",
            path.replace('/', "%2F")
        );
        let (rc, out) = gitlab_api(url, &api, None, &[]);
        if rc != 0 {
            anyhow::bail!("glab api {api} failed (rc={rc}): {out}");
        }
        let mr: GlMergeRequest = serde_json::from_str(out.trim())
            .map_err(|e| anyhow::anyhow!("glab api {api}: unparseable MR: {e}"))?;
        let cross_repo = mr.source_project_id != mr.target_project_id;
        let head_owner = match mr.source_project_id.filter(|_| cross_repo) {
            Some(source) => gitlab_project_owner(url, source)?,
            None => String::new(),
        };
        Ok(PrMeta {
            state: gitlab_norm_state(&mr.state),
            head_sha: mr.sha,
            head_ref: mr.source_branch,
            base_ref: mr.target_branch,
            author: mr.author.map_or_default(|a| a.username),
            title: mr.title,
            body: mr.description,
            cross_repo,
            head_owner,
        })
    }

    fn list_open_prs(&self, url: &str) -> anyhow::Result<Vec<OpenPr>> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let api = format!(
            "projects/{}/merge_requests?state=opened&per_page=100",
            path.replace('/', "%2F")
        );
        let (rc, out) = gitlab_api(url, &api, None, &[]);
        if rc != 0 {
            anyhow::bail!("glab api {api} failed (rc={rc}): {out}");
        }
        let mrs: Vec<GlMergeRequest> = serde_json::from_str(out.trim())
            .map_err(|e| anyhow::anyhow!("glab api {api}: unparseable MR list: {e}"))?;
        Ok(mrs
            .into_iter()
            .map(|mr| OpenPr {
                number: mr.iid,
                title: mr.title,
                author: mr.author.map_or_default(|a| a.username),
                is_draft: mr.draft,
                cross_repo: mr.source_project_id != mr.target_project_id,
                url: mr.web_url,
            })
            .collect())
    }

    fn can_push(&self, url: &str, login: &str) -> anyhow::Result<bool> {
        let (host, project) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let scope = format!("{}/{project}", host.to_ascii_lowercase());
        // GitLab tells any member who can push, so this is the only check
        // of hunter's own: without push access it cannot resolve the
        // threads of the reviews it posts, and every post would fail.
        let own = cached_lookup(format!("gitlab-push-self:{scope}"), || {
            gitlab_can_push(url, project, gitlab_own_id(url)?)
        })?;
        if !own {
            anyhow::bail!(
                "hunter's forge login cannot push to {project}: hunter needs push access \
                 to every repo it tracks, reviewed ones included"
            );
        }
        let key = format!("gitlab-push-login:{scope}:{}", login.to_ascii_lowercase());
        cached_lookup(key, || match gitlab_user_id(url, login)? {
            Some(id) => gitlab_can_push(url, project, id),
            None => Ok(false),
        })
    }

    fn pr_web_url(&self, url: &str, pr_number: i64) -> String {
        format!("{}/-/merge_requests/{pr_number}", repo_web_url(url))
    }

    fn pr_head_refspec(&self, pr_number: i64) -> String {
        format!(
            "+refs/merge-requests/{pr_number}/head:{}",
            pr_head_ref(pr_number)
        )
    }

    fn post_review(
        &self,
        url: &str,
        pr_number: i64,
        post: &ReviewPost,
    ) -> anyhow::Result<PostedReview> {
        let before = |e: anyhow::Error| e.context(NothingPosted);
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| before(anyhow::anyhow!("cannot parse GitLab URL: {url}")))?;
        let enc = path.replace('/', "%2F");
        let api = format!("projects/{enc}/merge_requests/{pr_number}");
        let (rc, out) = gitlab_api(url, &api, None, &[]);
        if rc != 0 {
            return Err(before(anyhow::anyhow!(
                "glab api {api} failed (rc={rc}): {out}"
            )));
        }
        let mr: GlMergeRequest = serde_json::from_str(out.trim())
            .map_err(|e| before(anyhow::anyhow!("glab api {api}: unparseable MR: {e}")))?;
        let refs = mr.diff_refs.unwrap_or_default();
        // A position names the MR's current version, which is not the one
        // the review read once the author pushed again: its lines would
        // land on whatever the new version has there.
        let moved;
        let post = if refs.head_sha == post.commit_id {
            post
        } else {
            moved = post.clone().without_inline();
            &moved
        };
        // A failure leaves the drafts it did not publish for the caller to
        // delete ([`Forge::delete_own_drafts`]).
        gitlab_publish_review(url, &enc, pr_number, (post, &refs))
    }

    fn delete_own_drafts(&self, url: &str, pr_number: i64) -> anyhow::Result<usize> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        gitlab_delete_own_drafts(url, &enc, pr_number)
    }

    fn find_review(
        &self,
        url: &str,
        pr_number: i64,
        step: &str,
        comments: &[(i64, &str)],
    ) -> anyhow::Result<FoundReview> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        let own_id = gitlab_own_id(url)?;
        let discussions = gitlab_discussions(url, &enc, pr_number)?;
        Ok(gitlab_find_review(&discussions, own_id, step, comments))
    }

    fn inline_comment_refs(
        &self,
        url: &str,
        pr_number: i64,
        _review_ref: &str,
        comments: &[(i64, &str)],
    ) -> anyhow::Result<Vec<(i64, String)>> {
        // A published note belongs to no review the API names: the MR's
        // discussions are read, and only the login's own notes carrying
        // the issue's mark are its ([`gitlab_published_refs`]).
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        let own_id = gitlab_own_id(url)?;
        let discussions = gitlab_discussions(url, &enc, pr_number)?;
        Ok(gitlab_published_refs(&discussions, own_id, comments))
    }

    fn review_threads(
        &self,
        url: &str,
        pr_number: i64,
        first_refs: &[&str],
        pr_author: &str,
        bots: &ReviewBots,
    ) -> anyhow::Result<Vec<ReviewThread>> {
        gitlab_review_threads(url, pr_number, first_refs, pr_author, bots)
    }

    fn reply_to_thread(
        &self,
        url: &str,
        pr_number: i64,
        reply_to: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        let api = format!("projects/{enc}/merge_requests/{pr_number}/discussions/{reply_to}/notes");
        let body = gitlab_text(&mark_own(body));
        let (rc, out) = gitlab_api(url, &api, Some("POST"), &[("body", &body)]);
        if rc != 0 {
            anyhow::bail!("glab api POST {api} failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn set_thread_resolved(
        &self,
        url: &str,
        pr_number: i64,
        thread_id: &str,
        resolved: bool,
    ) -> anyhow::Result<()> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        let api = format!(
            "projects/{enc}/merge_requests/{pr_number}/discussions/{thread_id}?resolved={resolved}"
        );
        let (rc, out) = gitlab_api(url, &api, Some("PUT"), &[]);
        if rc != 0 {
            anyhow::bail!("glab api PUT {api} failed (rc={rc}): {out}");
        }
        Ok(())
    }
}

/// GitLab's [`Forge::view_pr_sync`], with its notes required when
/// `strict_notes` ([`Forge::view_pr_comments`]): a notes read that fails is
/// then the view failing, not an MR without notes; otherwise it is a view
/// whose [`PrView::feedback_unread`] is set.
fn gitlab_view_sync(
    url: &str,
    pr_number: i64,
    bots: &ReviewBots,
    strict_notes: bool,
) -> anyhow::Result<PrView> {
    let (_, path) =
        gitlab_host_path(url).ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
    let (mr, notes) = gitlab_fetch_mr(url, path, pr_number, strict_notes)?;
    let comments = gitlab_feedback(url, path, notes.as_deref().unwrap_or_default(), bots)?;
    let state = gitlab_norm_state(&mr.state);
    let mergeable = gitlab_norm_mergeable(&mr);
    let status_check_rollup = gitlab_norm_pipeline(&mr);
    Ok(PrView {
        state,
        mergeable,
        review_decision: ReviewDecision::None,
        head_ref: mr.source_branch,
        head_sha: mr.sha,
        updated_at: mr.updated_at,
        title: String::new(),
        body: String::new(),
        status_check_rollup,
        comments,
        reviews: Vec::new(),
        feedback_unread: notes.is_none(),
    })
}

// ---------------------------------------------------------------------------
// Factory (`forge.forge_for`, `forge.detect_forge`)
// ---------------------------------------------------------------------------

/// Factory: pick the right Forge from a repo's forge field.
pub fn forge_for(forge: ForgeName) -> Box<dyn Forge> {
    match forge {
        ForgeName::Gitlab => Box::new(GitLabForge),
        ForgeName::Github => Box::new(GitHubForge),
    }
}

/// Split `host[:port]` at the port colon, keeping a bracketed IPv6
/// literal (`[fd00::5]:8443`) whole.
fn split_port(authority: &str) -> (&str, Option<&str>) {
    if authority.starts_with('[')
        && let Some(end) = authority.find(']')
    {
        let (host, rest) = authority.split_at(end + 1);
        return (host, rest.strip_prefix(':'));
    }
    match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    }
}

/// Split an scp-like `[user@]host:path` at the colon after the host,
/// skipping the colons inside a bracketed IPv6 literal. The host half may
/// not contain a slash.
fn scp_split(url: &str) -> Option<(&str, &str)> {
    let at = url.find('@').map_or(0, |i| i + 1);
    let colon = if url[at..].starts_with('[') {
        at + url[at..].find("]:")? + 1
    } else {
        url.find(':')?
    };
    let (a, p) = (&url[..colon], &url[colon + 1..]);
    (!a.contains('/')).then_some((a, p))
}

/// Host of a git remote URL: `https://`, `http://`, `ssh://` and the
/// scp-like `git@host:path` form, with any `user@` and `:port` stripped.
///
/// One parser, because the alternative is what this replaced: a
/// substring test against the whole URL, which called a GitHub repo
/// named `gitlab-ci-templates` a GitLab repo.
pub fn url_host(url: &str) -> Option<&str> {
    let after_scheme = strip_any_scheme(url);
    let authority = match after_scheme {
        // scheme://[user@]host[:port]/path
        Some(rest) => rest.split('/').next()?,
        // scp-like: [user@]host:path — the colon separates the path,
        // not a port, so split there and not on ':'. The authority half
        // may not contain a slash: `github.com/x:y` is not scp syntax,
        // and reading it as host `github.com/x` split this daemon from
        // the Python one, which rejects it — the same POST would store
        // forge=github here and forge=gitlab there.
        None => scp_split(url).map(|(a, _)| a)?,
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Only scheme URLs can carry a port; in the scp form the colon is
    // already gone with the path.
    let host = if after_scheme.is_some() {
        split_port(host).0
    } else {
        host
    };
    (!host.is_empty()).then_some(host)
}

/// Where the forge behind `url` answers on the web, as `host` or
/// `host:port` for [`crate::reach::first_unreachable`]: the host of
/// [`url_host`], with the port of an `http(s)://` URL that names one, and
/// 80 for a plain `http://` URL that does not.
///
/// An `ssh://` port is git's, not the web API's, which `gh`/`glab` still
/// reach on the host's HTTPS port; the scp form has no port at all.
pub fn web_host(url: &str) -> Option<String> {
    let host = url_host(url)?;
    let (rest, default_port) = match strip_scheme(url, "https") {
        Some(rest) => (Some(rest), None),
        None => (strip_scheme(url, "http"), Some("80")),
    };
    let port = rest
        .and_then(|rest| rest.split('/').next())
        .map(|authority| authority.rsplit_once('@').map_or(authority, |(_, h)| h))
        .and_then(|authority| split_port(authority).1)
        .filter(|port| port.parse::<u16>().is_ok())
        .or(rest.and(default_port));
    Some(port.map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")))
}

/// Which forge serves `url`, from its host alone.
///
/// The asymmetry is the point: GitHub is only ever reachable at domains
/// GitHub itself operates — `github.com` and GitHub Enterprise Cloud's
/// `<org>.ghe.com` — plus Enterprise Server installs, which by
/// convention carry a `github` label (`github.corp.com`). GitLab has no
/// such bound: any hostname at all can be a self-hosted GitLab, and
/// `code.corp.com` or `git.corp.com` usually is one. So GitHub gets the
/// closed list and GitLab gets the remainder.
///
/// Matching is per label rather than by substring, so a host merely
/// containing the letters (`notgithub.com`) is not GitHub. That is a
/// sanity bound, not an anti-spoofing measure: the URL comes from the
/// operator adding their own repo, and `github.com.evil.example` would
/// still read as GitHub.
///
/// This is only the fallback. An operator who runs something else, or a
/// GitHub Enterprise Server at a host that hides it, passes `forge`
/// explicitly on POST /api/repos and never reaches this function.
pub fn detect_forge(url: &str) -> ForgeName {
    if is_github_host(url_host(url).unwrap_or_default()) {
        ForgeName::Github
    } else {
        ForgeName::Gitlab
    }
}

#[cfg(test)]
mod run_cmd_cwd_tests {
    use super::*;

    /// The working directory is the whole point of this wrapper.
    /// Deliberately not the test's own cwd: a delegation that dropped the
    /// directory would still pass against a path the process is in anyway.
    #[test]
    fn test_command_runs_in_the_requested_directory() {
        let (rc, out) = run_cmd_cwd(&["sh", "-c", "pwd -P"], Path::new("/"), 30);
        assert_eq!(rc, 0);
        assert_eq!(out.trim_end(), "/");
    }

    /// The delegation has to carry the process-group reap with it, not
    /// just the cwd. A child that exits promptly while leaving a
    /// descendant holding the inherited pipes hangs `join_pipes` forever
    /// on this path, where no deadline is in force to rescue it — the bug
    /// the copied loop here shipped for two rounds.
    #[test]
    fn test_prompt_exit_with_a_lingering_descendant_does_not_block() {
        let t0 = std::time::Instant::now();
        let (rc, _out) = run_cmd_cwd(&["sh", "-c", "sleep 30 & exit 0"], Path::new("/"), 60);
        let elapsed = t0.elapsed();

        assert_eq!(rc, 0, "the direct child exited cleanly");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "run_cmd_cwd blocked on a descendant holding the pipes: {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod lookup_cache_tests {
    use super::*;

    /// An answer is reused for exactly [`LOOKUP_TTL`] and then asked
    /// again: a collaborator whose access was revoked stops being
    /// trusted once it runs out, not never.
    #[test]
    fn an_answer_is_fresh_until_the_ttl_and_stale_from_it() {
        let t0 = Instant::now();
        let mut cache = LookupCache::default();
        cache.record("github-push:h/o/r:lead".to_owned(), true, t0);

        let just_before = t0 + LOOKUP_TTL.saturating_sub(Duration::from_nanos(1));
        assert_eq!(cache.fresh("github-push:h/o/r:lead", t0), Some(true));
        assert_eq!(
            cache.fresh("github-push:h/o/r:lead", just_before),
            Some(true)
        );
        assert_eq!(cache.fresh("github-push:h/o/r:lead", t0 + LOOKUP_TTL), None);
        assert_eq!(cache.fresh("github-push:h/o/r:other", t0), None);
    }

    /// The cache holds what was asked in the last [`LOOKUP_TTL`], not
    /// everything ever asked: answers per comment or review would
    /// otherwise pile up for as long as the daemon runs.
    #[test]
    fn recording_drops_expired_answers_and_keeps_fresh_ones() {
        let t0 = Instant::now();
        let mut cache = LookupCache::default();
        cache.record("github-bot-item:h:IC_old".to_owned(), true, t0);
        let later = t0 + LOOKUP_TTL.saturating_sub(Duration::from_secs(1));
        cache.record("github-bot-item:h:IC_recent".to_owned(), true, later);

        cache.record(
            "github-bot-item:h:IC_new".to_owned(),
            false,
            t0 + LOOKUP_TTL,
        );

        let mut kept: Vec<&str> = cache.0.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(
            kept,
            ["github-bot-item:h:IC_new", "github-bot-item:h:IC_recent"]
        );
    }

    /// A batch (one GraphQL answer for many items) keeps each answer as
    /// given and, like a single answer, drops what has expired.
    #[test]
    fn a_batch_keeps_every_answer_and_drops_expired_ones() {
        let t0 = Instant::now();
        let mut cache = LookupCache::default();
        cache.record("github-bot-item:h:IC_old".to_owned(), true, t0);

        let now = t0 + LOOKUP_TTL;
        cache.record_all(
            [
                ("github-bot-item:h:IC_bot".to_owned(), true),
                ("github-bot-item:h:IC_user".to_owned(), false),
            ],
            now,
        );

        assert_eq!(cache.fresh("github-bot-item:h:IC_bot", now), Some(true));
        assert_eq!(cache.fresh("github-bot-item:h:IC_user", now), Some(false));
        assert_eq!(cache.0.len(), 2, "the expired answer is gone");
    }
}

#[cfg(test)]
mod gitlab_norm_state_tests {
    use super::*;

    /// `locked` is the MR mid-merge, not an MR that ended. Read as closed,
    /// `sync_prs` took a merging fix off `pr_open` as closed without merge,
    /// so the sync that would have seen it merged never came.
    #[test]
    fn a_locked_mr_is_still_open() {
        assert_eq!(gitlab_norm_state("locked"), PrState::Open);
        assert_eq!(gitlab_norm_state("opened"), PrState::Open);
        assert_eq!(gitlab_norm_state("merged"), PrState::Merged);
        assert_eq!(gitlab_norm_state("closed"), PrState::Closed);
    }
}

#[cfg(test)]
mod read_back_tests {
    use super::*;

    /// A discussion opened by note `id` of user `author`, on the diff.
    fn thread(id: i64, author: i64, body: &str) -> GlDiscussion {
        GlDiscussion {
            id: format!("d{id}"),
            notes: vec![GlNote {
                id: Some(id),
                body: body.to_owned(),
                author: Some(GlAuthor {
                    id: Some(author),
                    username: String::new(),
                }),
                position: Some(GlPosition::default()),
                ..GlNote::default()
            }],
        }
    }

    /// Each comment gets the note published for its issue, found by the
    /// issue's mark: never another issue's note of the same text, be it
    /// of the same post (published before a failure) or an earlier
    /// round's, and never somebody else's copy, however new.
    #[test]
    fn gitlab_finds_each_issues_note_by_its_mark() {
        let note = gitlab_inline_note;
        let discussions = [
            thread(12, 1, &note(102, "same")),
            thread(13, 2, &note(100, "same")),
            thread(5, 1, &note(7, "same")),
            thread(11, 1, &note(100, "same")),
            thread(10, 1, &note(101, "other")),
        ];
        let comments = [(100, "same"), (101, "other"), (102, "same")];
        assert_eq!(
            gitlab_published_refs(&discussions, 1, &comments),
            [
                (100, "11".to_owned()),
                (101, "10".to_owned()),
                (102, "12".to_owned())
            ]
        );
        assert_eq!(
            gitlab_published_refs(&discussions, 1, &[(102, "same")]),
            [(102, "12".to_owned())]
        );
        // Issue 100's own note is not on the MR: the notes of the same
        // text, issue 102's of its round and issue 7's of an earlier one,
        // are neither its.
        let without = [
            thread(12, 1, &note(102, "same")),
            thread(5, 1, &note(7, "same")),
        ];
        assert_eq!(
            gitlab_published_refs(&without, 1, &[(100, "same")]),
            Vec::<(i64, String)>::new()
        );
    }

    /// GitHub's comments of one review match the same way, in the order
    /// of their ids, not of the listing.
    #[test]
    fn github_tells_two_comments_of_one_text_apart_by_creation() {
        let listed = [
            (8, "IC_b".to_owned(), "same"),
            (7, "IC_a".to_owned(), "same"),
        ];
        assert_eq!(
            match_by_text(&[(1, "same"), (2, "same")], listed),
            [(1, "IC_a".to_owned()), (2, "IC_b".to_owned())]
        );
    }
}
