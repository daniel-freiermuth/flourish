//! Forge abstraction — GitHub / GitLab PR/MR lifecycle via CLI tools.
//! Port of hunter/forge.py. Methods shell out to `gh` / `glab`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::domain::ForgeName;
use crate::util::{run_cmd, run_cmd_in};

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
            "FAILURE" => Self::Failure,
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

/// Comment from GitHub's `pr.comments` array.
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
    /// Set by [`screen_feedback`]; never read from the forge.
    #[serde(skip)]
    pub voice: Voice,
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlAuthor {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub username: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlNote {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub author: Option<GlAuthor>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub system: bool,
    #[serde(default, rename = "type")]
    pub note_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GlPipeline {
    #[serde(default)]
    status: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GlMergeRequest {
    #[serde(default)]
    state: String,
    #[serde(default)]
    detailed_merge_status: Option<String>,
    #[serde(default)]
    merge_status: Option<String>,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    head_pipeline: Option<GlPipeline>,
    #[serde(default)]
    pipeline: Option<GlPipeline>,
}

/// One file of `merge_requests/:iid/diffs`: GitLab returns the hunks
/// without the file headers, which [`gitlab_unified_diff`] puts back.
#[derive(Debug, Clone, Default, Deserialize)]
// The bools are GitLab's own independent per-file flags, deserialized
// as sent; folding them into an enum would invent states the API lacks.
#[allow(clippy::struct_excessive_bools)]
struct GlFileDiff {
    #[serde(default)]
    old_path: String,
    #[serde(default)]
    new_path: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    deleted_file: bool,
    #[serde(default)]
    diff: String,
    /// GitLab withholds the hunks of a file over its diff size limits
    /// (`too_large`) or past its per-MR display limits (`collapsed`) and
    /// sends an empty `diff` for it; without these the file reads as
    /// changing nothing.
    #[serde(default)]
    too_large: bool,
    #[serde(default)]
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
    let mut comments = Vec::with_capacity(pr.comments.len());
    for mut c in std::mem::take(&mut pr.comments) {
        if let Some(voice) = voice_of(&c.id, c.author.as_ref())? {
            c.voice = voice;
            comments.push(c);
        }
    }
    let mut reviews = Vec::with_capacity(pr.reviews.len());
    for mut r in std::mem::take(&mut pr.reviews) {
        if let Some(voice) = voice_of(&r.id, r.author.as_ref())? {
            r.voice = voice;
            reviews.push(r);
        }
    }
    pr.comments = comments;
    pr.reviews = reviews;
    Ok(())
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
/// (a deleted account) is a 404 -- a "no", not an error.
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

/// Screen a GitHub PR view: see [`screen_feedback`].
fn github_screen(url: &str, pr: &mut PrView, bots: &ReviewBots) -> anyhow::Result<()> {
    let (host, repo) = url_host_path(url).map_or_else(
        || (String::new(), url.to_owned()),
        |(host, path)| {
            let host = host.to_ascii_lowercase();
            let repo = format!("{host}/{path}");
            (host, repo)
        },
    );
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

/// A GitLab MR's notes as screened (comments, reviews): see
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
) -> anyhow::Result<(Vec<GhComment>, Vec<GhReview>)> {
    let ids: HashMap<&str, i64> = notes
        .iter()
        .filter_map(|n| n.author.as_ref())
        .filter_map(|a| Some((a.username.as_str(), a.id?)))
        .collect();
    let (comments, reviews) = gitlab_split_notes(notes);
    let mut view = PrView {
        comments,
        reviews,
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
    Ok((view.comments, view.reviews))
}

pub trait Forge: Send + Sync {
    /// Git SSH URL for push operations.
    fn ssh_url(&self, https_url: &str) -> String;
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
    /// Heavier PR view for engage (includes title/body), its feedback
    /// screened ([`screen_feedback`]).
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
    /// Post a comment on a PR/MR.
    fn comment_pr(&self, url: &str, pr_number: i64, body: &str) -> anyhow::Result<()>;
    /// Close a PR/MR with a comment explaining why.
    fn close_pr(&self, url: &str, pr_number: i64, comment: &str) -> anyhow::Result<()>;
    /// Push to the remote (--force to raw SSH URL).
    fn push(&self, repo_path: &Path, url: &str, branch: &str) -> anyhow::Result<()>;
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

/// Parse owner/repo from a GitHub-style URL (HTTPS or SSH).
///
/// For an Enterprise host the owner half carries the host, giving
/// `gh`'s `[HOST/]OWNER/REPO` form — every consumer formats the pair
/// straight into `-R`, and without the host `gh` would talk to
/// github.com about a repo that only exists on the internal instance.
fn github_owner_repo(url: &str) -> Option<(String, String)> {
    // Every form `valid_repo_url` accepts, not just the two most common:
    // an `ssh://` or `http://` remote that the add endpoint takes with a
    // 201 must not then be unparseable here, or the repo gets a PR that
    // `sync_prs` can never track.
    let (host, path) = url_host_path(url)?;
    if !is_github_host(host) {
        return None;
    }
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
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

/// Whether `url` carries one of the web schemes `ssh_url` rewrites.
///
/// `http://` counts alongside `https://`: `valid_repo_url` accepts it,
/// and a plain-HTTP remote carries none of the SSH key material the
/// push path depends on, so leaving it unrewritten sent
/// `git push --force` over an unauthenticated, unencrypted hop.
fn is_web_scheme(url: &str) -> bool {
    strip_scheme(url, "https").is_some() || strip_scheme(url, "http").is_some()
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
        None => url
            .split_once(':')
            .filter(|(a, _)| !a.contains('/'))
            .map(|(_, p)| p)?,
    };
    (!path.is_empty()).then_some((host, path))
}

/// Extract (host, `project_path`) from a GitLab-style URL.
fn gitlab_host_path(url: &str) -> Option<(&str, &str)> {
    let (host, path) = url_host_path(url)?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    (!path.is_empty()).then_some((host, path))
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
    })
}

/// Most pages of an MR's diff that `pr_diff` reads from GitLab: 1,000
/// files. The harvest prompt keeps only the first
/// [`crate::playbooks::PR_DIFF_CAP_CHARS`] characters of the diff, which
/// far fewer files already fill, so fetching past this buys the worker
/// nothing — and a server that ignored `page` would otherwise be asked
/// forever.
pub const MAX_DIFF_PAGES: usize = 10;

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

fn gitlab_norm_state(raw: &str) -> PrState {
    match raw.to_ascii_lowercase().as_str() {
        "merged" => PrState::Merged,
        "closed" | "locked" => PrState::Closed,
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

/// Split GitLab notes into (comments, reviews) normalized to Gh types.
fn gitlab_split_notes(notes: &[GlNote]) -> (Vec<GhComment>, Vec<GhReview>) {
    let mut comments = Vec::new();
    let mut reviews = Vec::new();
    for n in notes {
        if n.system {
            continue;
        }
        let ts = if n.created_at.is_empty() {
            &n.updated_at
        } else {
            &n.created_at
        };
        let author = n.author.as_ref().map(|a| GhAuthor {
            login: a.username.clone(),
        });
        if n.note_type.as_deref() == Some("DiffNote") {
            reviews.push(GhReview {
                // Only GitHub screening asks by item; see `gitlab_feedback`.
                id: String::new(),
                submitted_at: ts.clone(),
                body: n.body.clone(),
                author,
                state: String::new(),
                voice: Voice::default(),
            });
        } else {
            comments.push(GhComment {
                id: String::new(),
                created_at: ts.clone(),
                body: n.body.clone(),
                author,
                voice: Voice::default(),
            });
        }
    }
    (comments, reviews)
}

/// Fetch GitLab MR + notes via `glab api`, returning typed structs.
fn gitlab_fetch_mr(
    url: &str,
    slug: &str,
    number: i64,
) -> anyhow::Result<(GlMergeRequest, Vec<GlNote>)> {
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
    let mut notes: Vec<GlNote> = if rc2 == 0 {
        serde_json::from_str(out2.trim()).unwrap_or_default()
    } else {
        Vec::new()
    };
    notes.reverse();
    Ok((mr, notes))
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

/// URL of the open MR a refused `glab mr create` says already exists.
///
/// GitLab names it by reference only — "Another open merge request already
/// exists for this source branch: !12" (`MergeRequest#conflicting_mr_message`)
/// — so the URL `run_fix` recovers onto has to be looked up. `None` for any
/// other failure, or when the lookup fails.
fn gitlab_existing_mr_url(repo_path: &Path, create_out: &str) -> Option<String> {
    let (_, reference) = create_out.split_once("already exists for this source branch: !")?;
    let iid: String = reference.chars().take_while(char::is_ascii_digit).collect();
    if iid.is_empty() {
        return None;
    }
    let (rc, out) = run_cmd_cwd(
        &["glab", "mr", "view", &iid, "--output", "json"],
        repo_path,
        60,
    );
    if rc != 0 {
        return None;
    }
    let mr: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    let url = mr.get("web_url")?.as_str()?;
    url.contains("/-/merge_requests/").then(|| url.to_owned())
}

// ---------------------------------------------------------------------------
// GitHub (via `gh` CLI) — `forge.GitHubForge`
// ---------------------------------------------------------------------------

pub struct GitHubForge;
pub struct GitLabForge;

impl Forge for GitHubForge {
    fn ssh_url(&self, https_url: &str) -> String {
        // Enterprise hosts too: a self-hosted GitHub's clone URL is
        // `git@<its host>:owner/repo.git`, and rewriting it to
        // github.com would push an internal repo at the public forge.
        if is_web_scheme(https_url)
            && let Some((host, path)) = url_host_path(https_url)
            && is_github_host(host)
        {
            let path = path.trim_end_matches('/').trim_end_matches(".git");
            if !path.is_empty() {
                return format!("git@{host}:{path}.git");
            }
        }
        https_url.to_owned()
    }

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
        // PR URL is the last non-empty line of output.
        let url = out.trim().lines().last().unwrap_or("").to_owned();
        Ok(url)
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
            "title,body,comments,reviews,statusCheckRollup,headRefName,headRefOid",
        )?;
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
            &["gh", "pr", "comment", &num, "-R", &slug, "--body", body],
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
        let ssh = self.ssh_url(url);
        let refspec = format!("HEAD:{branch}");
        let (rc, out) = run_cmd_cwd(&["git", "push", "--force", &ssh, &refspec], repo_path, 120);
        if rc != 0 {
            anyhow::bail!("git push failed (rc={rc}): {out}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitLab (via `glab` CLI + REST API) — `forge.GitLabForge`
// ---------------------------------------------------------------------------

impl Forge for GitLabForge {
    fn ssh_url(&self, https_url: &str) -> String {
        if let Some((host, path)) = gitlab_host_path(https_url)
            && is_web_scheme(https_url)
        {
            return format!("git@{host}:{path}.git");
        }
        https_url.to_owned()
    }

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
                body,
                "--yes",
            ],
            repo_path,
            300,
        );
        if rc != 0 {
            if let Some(url) = gitlab_existing_mr_url(repo_path, &out) {
                anyhow::bail!(
                    "glab mr create failed (rc={rc}): {} (existing MR: {url})",
                    out.trim_end()
                );
            }
            anyhow::bail!("glab mr create failed (rc={rc}): {out}");
        }
        // glab prints the MR URL; search for it.
        for line in out.trim().lines().rev() {
            if line.contains("/-/merge_requests/") {
                return Ok(line.trim().to_owned());
            }
        }
        Ok(out.trim().lines().last().unwrap_or("").to_owned())
    }

    fn view_pr_sync(&self, url: &str, pr_number: i64, bots: &ReviewBots) -> anyhow::Result<PrView> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let (mr, notes) = gitlab_fetch_mr(url, path, pr_number)?;
        let (comments, reviews) = gitlab_feedback(url, path, &notes, bots)?;
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
            reviews,
        })
    }

    fn view_pr_engage(
        &self,
        url: &str,
        pr_number: i64,
        bots: &ReviewBots,
    ) -> anyhow::Result<PrView> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let (mr, notes) = gitlab_fetch_mr(url, path, pr_number)?;
        let (comments, reviews) = gitlab_feedback(url, path, &notes, bots)?;
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
            reviews,
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
        let (rc, out) = gitlab_api(url, &api_path, Some("POST"), &[("body", body)]);
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
        let ssh = self.ssh_url(url);
        let refspec = format!("HEAD:{branch}");
        let (rc, out) = run_cmd_cwd(&["git", "push", "--force", &ssh, &refspec], repo_path, 120);
        if rc != 0 {
            anyhow::bail!("git push failed (rc={rc}): {out}");
        }
        Ok(())
    }
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
        None => url
            .split_once(':')
            .map(|(a, _)| a)
            .filter(|a| !a.contains('/'))?,
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Only scheme URLs can carry a port; in the scp form the colon is
    // already gone with the path.
    let host = if after_scheme.is_some() {
        host.split_once(':').map_or(host, |(h, _)| h)
    } else {
        host
    };
    (!host.is_empty()).then_some(host)
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
