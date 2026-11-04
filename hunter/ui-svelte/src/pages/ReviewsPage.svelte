<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { POLL_MS, errorMessage, post, store } from "../lib/api.svelte";
  import { clock, datetime, isHttpUrl, issuesAwaitingJudgment, reviewIssueStatusLabel, reviewStateLabel, roundPostTally } from "../lib/format";
  import { toast } from "../lib/toast.svelte";
  import type { OpenPrOut, PrReviewOut, PrReviewRoundOut } from "../lib/types";

  // ── Review list: polled by this page only ─────────────────────────
  // Reviews are shown nowhere else, so the dashboard poll does not carry
  // them; this page polls on the dashboard's period while it is mounted.

  let reviews = $state<PrReviewOut[] | null>(null);
  let loadError = $state<string | null>(null);

  // Only the latest load may write: a poll and the reload after a write
  // can overlap, and the older answer must not replace the newer one.
  let loadGeneration = 0;
  // Set for the duration of a poll only, so a slow daemon does not stack
  // ticks; the reload after a write is not gated, like the store's.
  let pollInFlight = false;
  let timer: ReturnType<typeof setInterval> | null = null;

  function reason(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
  }

  async function load() {
    const gen = ++loadGeneration;
    try {
      const list = await store.fetchPrReviews();
      if (gen !== loadGeneration) return;
      reviews = list;
      loadError = null;
    } catch (err) {
      if (gen !== loadGeneration) return;
      loadError = reason(err);
    }
  }

  async function poll() {
    if (pollInFlight) return;
    pollInFlight = true;
    try {
      await load();
    } finally {
      pollInFlight = false;
    }
  }

  onMount(() => {
    void poll();
    timer = setInterval(() => void poll(), POLL_MS);
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  // A write changes what the scheduler picks next (the status panel's
  // "next up"), so the dashboard is refreshed along with the list.
  function reloadAfterWrite() {
    void load();
    void store.refresh();
  }

  /** A commit as the page shows it: the abbreviated sha, or a dash. */
  function short(sha: string | null): string {
    return sha ? sha.slice(0, 7) : "\u2013";
  }

  // ── Request form ──────────────────────────────────────────────────

  const repos = $derived(store.summary?.repos ?? []);
  let repoId = $state<number | null>(null);
  let prInput = $state("");
  let watch = $state(true);
  // Off by default: a post goes out under the operator's own forge login.
  let autoPost = $state(false);
  let submitting = $state(false);

  // The open-PR picker belongs to the repo it was loaded for; switching the
  // repo hides it rather than offering another repo's PRs.
  let picker = $state<{ repoId: number; prs: OpenPrOut[] | null } | null>(null);
  let pickerRequest = 0;
  const pickerOpen = $derived(picker !== null && picker.repoId === repoId);

  async function togglePicker() {
    if (pickerOpen) {
      picker = null;
      return;
    }
    const id = repoId;
    if (id === null) return;
    const request = ++pickerRequest;
    picker = { repoId: id, prs: null };
    try {
      const prs = await store.fetchOpenPrs(id);
      if (request !== pickerRequest) return;
      picker = { repoId: id, prs };
    } catch (err) {
      if (request !== pickerRequest) return;
      console.error(`list open PRs of repo ${id} failed`, err);
      toast(`Failed to list open PRs: ${reason(err)}`, false);
      picker = null;
    }
  }

  function pick(pr: OpenPrOut) {
    prInput = String(pr.number);
    picker = null;
  }

  /** The 409 body that asks for a vouch, if this is one. */
  function vouchRequest(body: unknown): { author: string; crossRepo: boolean } | null {
    if (typeof body !== "object" || body === null) return null;
    const b = body as Record<string, unknown>;
    if (b.needs_vouch !== true) return null;
    return {
      author: typeof b.author === "string" ? b.author : "The PR's author",
      crossRepo: b.cross_repo === true,
    };
  }

  async function requestReview() {
    const id = repoId;
    const pr = prInput.trim();
    if (id === null || !pr) {
      toast("Choose a repo and a PR", false);
      return;
    }
    submitting = true;
    try {
      const body: Record<string, unknown> = { repo_id: id, pr, watch, auto_post: autoPost };
      let r = await post<{ id?: number; outcome?: string }>("/api/pr-review", body);
      const vouch = r.status === 409 ? vouchRequest(r.body) : null;
      if (vouch) {
        // Reviewing runs the PR's code (tests) with the daemon's
        // credentials, so an untrusted author takes an explicit yes.
        const warning =
          `${vouch.author} cannot push to this repo. Reviewing runs their code with your credentials.`
          + (vouch.crossRepo
            ? " Vouching for them also vouches for anyone who can push to their fork."
            : "");
        if (!confirm(`${warning}\n\nVouch for ${vouch.author} and review?`)) return;
        r = await post<{ id?: number; outcome?: string }>("/api/pr-review", { ...body, vouch: true });
      }
      if (!r.ok) {
        console.error(`PR review request for ${pr} (repo ${id}) rejected with HTTP ${r.status}`);
        toast(errorMessage(r.body) ?? `Review request failed (HTTP ${r.status})`, false);
        return;
      }
      // `updated` (watch and auto-post set on a review that exists),
      // `reopened` (its PR was reopened), or `requested`.
      const outcome = typeof r.body?.outcome === "string" ? r.body.outcome : "requested";
      toast(`Review of PR ${pr} ${outcome}`, true);
      prInput = "";
      reloadAfterWrite();
    } catch (err) {
      console.error(`PR review request for ${pr} (repo ${id}) failed`, err);
      toast(`Review request failed: ${reason(err)}`, false);
    } finally {
      submitting = false;
    }
  }

  // ── Per-review writes ─────────────────────────────────────────────

  // Reviews with a write in flight: a second click would only repeat it.
  const busy = new SvelteSet<number>();

  async function reviewWrite(review: PrReviewOut, path: string, body: Record<string, unknown>, done: string) {
    if (busy.has(review.id)) return;
    busy.add(review.id);
    const what = `${review.repo_name} PR #${review.pr_number}`;
    try {
      const r = await post(path, body);
      if (!r.ok) {
        console.error(`${path} for review ${review.id} rejected with HTTP ${r.status}`);
        toast(errorMessage(r.body) ?? `Failed: ${what} (HTTP ${r.status})`, false);
        return;
      }
      toast(`${what}: ${done}`, true);
      reloadAfterWrite();
    } catch (err) {
      console.error(`${path} for review ${review.id} failed`, err);
      toast(`Failed: ${what} (${reason(err)})`, false);
    } finally {
      busy.delete(review.id);
    }
  }

  /**
   * Waiting for the scheduler: the head is unreviewed and the review
   * would take it. A watched review still waits for the head to stay
   * unchanged for a while; the badge's title says so.
   */
  function pending(review: PrReviewOut): boolean {
    return review.state === "open"
      && !review.held
      && review.running_job_id === null
      && review.head_sha !== null
      && review.head_sha !== review.last_reviewed_sha
      && (review.watch || review.last_reviewed_sha === null);
  }

  /** A round's range, or what a round without new commits ran for. */
  function roundLabel(round: PrReviewRoundOut): string {
    return round.base_sha === round.head_sha
      ? "re: replies"
      : `${short(round.base_sha)}..${short(round.head_sha)}`;
  }

  /** What the review is queued for when replies or reopens alone make it due. */
  function judgmentQueued(review: PrReviewOut): string {
    const n = issuesAwaitingJudgment(review);
    const what = n === 1 ? "1 issue to judge again" : `${n} issues to judge again`;
    return `${what} \u2014 review queued`;
  }

  /** Discarding is final, so it asks first. */
  function discardRound(review: PrReviewOut, round: PrReviewRoundOut) {
    if (!confirm(`Never post round ${roundLabel(round)} to the PR?`)) return;
    void reviewWrite(review, "/api/pr-review/discard", { round_id: round.id }, "round discarded");
  }
</script>

{#snippet roundView(review: PrReviewOut, round: PrReviewRoundOut)}
  {@const blocking = round.issues.filter((i) => i.blocking).length}
  <div class="round">
    <div class="round-head">
      <code
        class="range"
        class:replies-round={round.base_sha === round.head_sha}
        title={round.base_sha === round.head_sha
          ? `No new commits at ${round.head_sha}: this round judged the replies in the earlier issues' threads`
          : `${round.base_sha}..${round.head_sha}`}
      >{roundLabel(round)}</code>
      <span
        class="badge verdict-{round.verdict}"
        title={round.verdict === "partial"
          ? "The reviewer was stopped before it finished: its issues and verdicts count, but these commits are reviewed again"
          : undefined}
      >{round.verdict}</span>
      {#if round.verdict !== "failed"}
        <span class="issue-count" class:has-blocking={blocking > 0}>
          {round.issues.length} {round.issues.length === 1 ? "issue" : "issues"}{blocking > 0 ? ` (${blocking} blocking)` : ""}
        </span>
        {#if round.resolutions > 0}
          <!-- A round can resolve earlier issues without finding new ones;
               it is still posted, as replies that resolve their threads. -->
          <span class="resolution-count" title="Earlier issues this round found addressed or whose rebuttal it accepted; posting replies to and resolves their threads">
            {round.resolutions} {round.resolutions === 1 ? "resolution" : "resolutions"}
          </span>
        {/if}
        {#if round.answers > 0}
          <!-- Replies in threads of issues the round found still open get
               an answer too; those threads stay open. -->
          <span class="answer-count" title="Earlier issues this round found still open where somebody replied, or where hunter's last answer settled them; posting answers each thread and leaves it open">
            {round.answers} {round.answers === 1 ? "answer" : "answers"}
          </span>
        {/if}
      {/if}
      <span class="dim">{clock(round.finished_at)}</span>
      {#if round.job_id !== null}
        <span class="dim">job #{round.job_id}</span>
      {/if}
      {#if round.post_state === "pending"}
        <span class="badge post-pending" title="Waiting for a decision: nothing is posted without one">{round.landed ? "partly posted" : "not posted"}</span>
        <button
          class="retry-btn"
          disabled={busy.has(review.id) || review.state !== "open"}
          onclick={() => reviewWrite(review, "/api/pr-review/post", { round_id: round.id }, "posting to the PR")}
          title="Post this round to the PR under your forge login: its issues as one comment review, then an answer in each thread whose issue it judged differently from what the thread says, resolving the threads of settled issues"
        >
          Post to PR ({roundPostTally(round.issues.length, round.resolutions, round.answers)})
        </button>
        {#if !round.landed}
          <button
            class="discard-btn"
            disabled={busy.has(review.id)}
            onclick={() => discardRound(review, round)}
          >
            Discard
          </button>
        {/if}
      {:else if round.post_state === "approved"}
        {#if review.state === "open"}
          <span class="badge accent" title="Posted on the next refresh of the reviews, at once after an approval">posting…</span>
        {:else}
          <span class="badge" title="Only an open review's rounds are posted">not posted ({reviewStateLabel(review.state)})</span>
        {/if}
      {:else if round.post_state === "posted"}
        <span class="badge verdict-ok">posted{round.posted_at !== null ? ` ${clock(round.posted_at)}` : ""}</span>
      {:else if round.post_state === "discarded"}
        <span class="badge">discarded</span>
      {/if}
    </div>
    {#if round.failure}
      <p class="round-failure">{round.failure}</p>
    {/if}
    {#if round.post_error && (round.post_state === "pending" || round.post_state === "approved")}
      <p class="round-failure">
        {round.post_state === "pending" ? "Posting failed; approve to try again:" : "Posting failed, retrying:"}
        {round.post_error}
      </p>
    {/if}
    {#if round.issues.length > 0}
      <ul class="issues">
        {#each round.issues as issue (issue.id)}
          <li class="issue" class:blocking={issue.blocking} class:settled={issue.status === "addressed" || issue.status === "rebutted"}>
            <div class="issue-head">
              <span class="chip status-{issue.status}" title="As judged by this PR's later rounds">
                {reviewIssueStatusLabel(issue.status)}
              </span>
              <span class="chip" class:chip-blocking={issue.blocking}>
                {issue.blocking ? "blocking" : "non-blocking"}
              </span>
              {#if issue.severity}
                <span class="chip sev-{issue.severity}">{issue.severity}</span>
              {/if}
              <span class="chip">{issue.kind}</span>
              {#if issue.file}
                <code class="location">{issue.file}{issue.line !== null ? `:${issue.line}` : ""}</code>
              {/if}
              {#if issue.posted_inline}
                <span class="chip chip-inline" title="Posted as a comment on this line of the PR's diff">inline</span>
              {/if}
              {#if issue.resolved_on_forge}
                <span class="chip chip-resolved" title="Hunter replied to this issue's thread on the PR and resolved it">resolved on forge</span>
              {/if}
            </div>
            {#if issue.detail}
              <details class="issue-detail">
                <summary>{issue.summary}</summary>
                <pre>{issue.detail}</pre>
              </details>
            {:else}
              <p class="issue-summary">{issue.summary}</p>
            {/if}
            {#if issue.verdict_note}
              <p class="verdict-note" title="The latest round's judgement of this issue">{issue.verdict_note}</p>
            {/if}
            {#if issue.posted_inline && (issue.replies.length > 0 || issue.hidden_replies > 0)}
              <ul class="replies">
                {#each issue.replies as reply, i (i)}
                  <li class="reply" class:own={reply.own}>
                    <span class="reply-author">{reply.author}</span>
                    <p class="reply-body">{reply.body}</p>
                  </li>
                {/each}
                {#if issue.hidden_replies > 0}
                  <!-- Screening dropped replies from authors who cannot
                       vouch for the PR; their text never reaches the UI. -->
                  <li class="reply-hidden dim" title="Replies from authors who are not pushers, review bots or the PR author">
                    {issue.hidden_replies} hidden {issue.hidden_replies === 1 ? "reply" : "replies"}
                  </li>
                {/if}
              </ul>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{/snippet}

<div class="page-enter">
  <div class="page">
    <h2 class="page-title">Reviews</h2>

    <!-- Request form -->
    <div class="request-card">
      <div class="request-row">
        <label class="field-label repo-field">
          repo
          <select bind:value={repoId} class="field-input">
            <option value={null} disabled>choose a repo</option>
            {#each repos as repo (repo.id)}
              <option value={repo.id}>{repo.name}</option>
            {/each}
          </select>
        </label>
        <label class="field-label pr-field">
          PR
          <input
            type="text"
            placeholder="PR URL or number"
            bind:value={prInput}
            class="field-input"
            onkeydown={(e) => { if (e.key === "Enter") void requestReview(); }}
          />
        </label>
        <label class="watch-field" title="Review every new head once it has stopped changing, not just the current one">
          <input type="checkbox" bind:checked={watch} />
          Watch
        </label>
        <label class="watch-field" title="Post each round's issues to the PR without waiting for your approval (under your forge login)">
          <input type="checkbox" bind:checked={autoPost} />
          Post automatically
        </label>
      </div>
      <div class="request-actions">
        <button
          class="pick-btn"
          onclick={togglePicker}
          disabled={repoId === null}
          aria-expanded={pickerOpen}
        >
          {pickerOpen ? "Hide open PRs" : "Pick from open PRs"}
        </button>
        <button class="submit-btn" onclick={requestReview} disabled={submitting || repoId === null || !prInput.trim()}>
          {submitting ? "Requesting…" : "Request review"}
        </button>
      </div>

      {#if pickerOpen && picker}
        <div class="picker">
          {#if picker.prs === null}
            <p class="dim">Loading open PRs…</p>
          {:else if picker.prs.length === 0}
            <p class="dim">No open PRs.</p>
          {:else}
            <ul class="pr-options">
              {#each picker.prs as pr (pr.number)}
                <li>
                  <button
                    class="pr-option"
                    disabled={pr.hunter_own}
                    onclick={() => pick(pr)}
                    title={pr.hunter_own ? "Hunter opened this PR; its own PRs are not reviewed here" : `Use PR #${pr.number}`}
                  >
                    <span class="pr-num">#{pr.number}</span>
                    <span class="pr-title">{pr.title}</span>
                    <span class="dim">{pr.author}</span>
                    {#if pr.is_draft}
                      <span class="badge">draft</span>
                    {/if}
                    {#if pr.cross_repo}
                      <span class="badge">fork</span>
                    {/if}
                    {#if pr.hunter_own}
                      <span class="badge">hunter's own</span>
                    {:else if pr.review_id !== null}
                      <span class="badge accent">reviewing</span>
                    {/if}
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      {/if}
    </div>

    {#if loadError}
      <div class="load-error" role="status">Cannot load reviews ({loadError}){reviews ? " — the list below may be stale." : ""}</div>
    {/if}

    <!-- Review list -->
    {#if reviews === null}
      {#if !loadError}
        <p class="dim">Loading reviews…</p>
      {/if}
    {:else if reviews.length === 0}
      <div class="empty-state">
        <div class="empty-icon">◎</div>
        <p class="empty-title">No reviews</p>
        <p class="empty-sub">Request a review of a PR above. A watched PR is reviewed again whenever new commits land.</p>
      </div>
    {:else}
      <div class="review-list">
        {#each reviews as review (review.id)}
          <div class="review-card">
            <div class="review-header">
              <div class="review-info">
                <div class="name-row">
                  <span class="repo-name">{review.repo_name}</span>
                  {#if isHttpUrl(review.pr_url)}
                    <a href={review.pr_url} target="_blank" rel="noopener" class="pr-link">#{review.pr_number}</a>
                  {:else}
                    <span class="pr-link">#{review.pr_number}</span>
                  {/if}
                  <span class="title">{review.title}</span>
                </div>
                <div class="meta">
                  by {review.author}
                  {#if review.vouched_author !== null}
                    <span class="badge vouched" title="Vouched for: {review.vouched_author} cannot push to this repo">vouched</span>
                  {/if}
                  {#if review.cross_repo}
                    <span class="badge">fork</span>
                  {/if}
                  <span class="meta-sep">·</span>
                  requested by {review.requested_by} {datetime(review.requested_at)}
                  <span class="meta-sep">·</span>
                  head <code>{short(review.head_sha)}</code>
                  <span class="meta-sep">·</span>
                  reviewed <code>{short(review.last_reviewed_sha)}</code>
                  {#if review.awaiting_since !== null}
                    <span class="meta-sep">·</span>
                    news to judge since {datetime(review.awaiting_since)}
                  {/if}
                </div>
                <div class="badge-row">
                  <span class="badge state-{review.state}">{reviewStateLabel(review.state)}</span>
                  {#if review.running_job_id !== null}
                    <span class="badge running">▶ reviewing · job #{review.running_job_id}</span>
                  {:else if pending(review)}
                    <span
                      class="badge accent"
                      title={review.last_reviewed_sha === null
                        ? "Waiting for the scheduler"
                        : "Waiting for the head to stay unchanged, then for the scheduler"}
                    >
                      pending
                    </span>
                  {:else if review.judgment_pending}
                    <span
                      class="badge accent"
                      title="Replies or reopened threads that no round has judged: a round judges them once the newest has been quiet for two minutes"
                    >
                      {judgmentQueued(review)}
                    </span>
                  {/if}
                  {#if review.held}
                    <span class="badge held">held</span>
                  {:else if review.failures > 0}
                    <span class="badge failing" title="Rounds in a row that failed or were stopped before they finished">unfinished {review.failures}×</span>
                  {/if}
                </div>
              </div>
              <div class="review-actions">
                <button
                  class="toggle-btn"
                  class:is-on={review.watch}
                  disabled={busy.has(review.id) || review.state !== "open"}
                  onclick={() => reviewWrite(
                    review, "/api/pr-review/watch", { id: review.id, watch: !review.watch },
                    review.watch ? "no longer watched" : "watched",
                  )}
                  aria-pressed={review.watch}
                  aria-label="Watch {review.repo_name} PR #{review.pr_number}"
                  title={review.watch
                    ? "Watched: every new head is reviewed. Click to stop."
                    : "Not watched: only the first head is reviewed. Click to review every new head."}
                >
                  {review.watch ? "Watching" : "Watch"}
                </button>
                <button
                  class="toggle-btn"
                  class:is-on={review.auto_post}
                  disabled={busy.has(review.id) || review.state !== "open"}
                  onclick={() => reviewWrite(
                    review, "/api/pr-review/auto-post", { id: review.id, auto_post: !review.auto_post },
                    review.auto_post ? "rounds wait for approval" : "rounds are posted automatically",
                  )}
                  aria-pressed={review.auto_post}
                  aria-label="Auto-post {review.repo_name} PR #{review.pr_number}"
                  title={review.auto_post
                    ? "New rounds are posted to the PR without approval. Click to approve each one."
                    : "New rounds wait for your approval. Click to post them automatically."}
                >
                  {review.auto_post ? "Auto-posting" : "Auto-post"}
                </button>
              </div>
            </div>

            {#if review.repo_moved}
              <div class="failure-panel held">
                <div class="failure-text">
                  <b>The repo's URL changed since this review was requested.</b>
                  PR #{review.pr_number} there may be another PR, so the review does nothing
                  until you request it again: the PR is then read at the new URL, its author
                  checked afresh, and the review starts over there, none of its rounds so far
                  counting.
                </div>
              </div>
            {/if}
            {#if review.held || (review.failures > 0 && review.last_failure)}
              <div class="failure-panel" class:held={review.held}>
                <div class="failure-text">
                  {#if review.held}
                    <b>Held after {review.failures} unfinished rounds.</b>
                  {/if}
                  {review.last_failure ?? ""}
                </div>
                {#if review.held}
                  <button
                    class="retry-btn"
                    disabled={busy.has(review.id)}
                    onclick={() => reviewWrite(review, "/api/pr-review/retry", { id: review.id }, "retry queued")}
                    aria-label="Retry {review.repo_name} PR #{review.pr_number}"
                  >
                    Retry
                  </button>
                {/if}
              </div>
            {/if}

            <!-- Rounds arrive newest first: the latest is what the PR looks
                 like now, the rest is history. -->
            {#if review.rounds[0]}
              {@const earlier = review.rounds.slice(1)}
              <div class="rounds">
                {@render roundView(review, review.rounds[0])}
                {#if earlier.length > 0}
                  <details class="earlier">
                    <summary>{earlier.length} earlier {earlier.length === 1 ? "round" : "rounds"}</summary>
                    {#each earlier as round (round.id)}
                      {@render roundView(review, round)}
                    {/each}
                  </details>
                {/if}
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>

<style>
  .page {
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
  }

  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
  }

  .dim { color: var(--text-dim); }

  code {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
  }

  /* ── Request form ────────────────────────────────────────────── */
  .request-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 0.75rem 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.625rem;
  }

  .request-row {
    display: flex;
    gap: 0.75rem;
    align-items: flex-end;
    flex-wrap: wrap;
  }

  .field-label {
    display: block;
    font-size: 0.75rem;
    color: var(--text-dim);
    font-weight: 500;
  }
  .repo-field { flex: 0 1 14rem; }
  .pr-field { flex: 1 1 18rem; }

  .field-input {
    margin-top: 0.1875rem;
    width: 100%;
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 0.375rem 0.625rem;
    font-size: 0.8125rem;
    color: var(--text);
    transition: border-color var(--transition);
  }
  .field-input::placeholder { color: rgba(136, 136, 136, 0.45); }
  .field-input:focus { border-color: var(--accent); outline: none; }

  .watch-field {
    display: flex;
    align-items: center;
    gap: 0.375rem;
    font-size: 0.8125rem;
    padding-bottom: 0.375rem;
  }

  .request-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
  }

  .pick-btn {
    font-size: 0.75rem;
    padding: 0.375rem 0.75rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    color: var(--text-dim);
  }
  .pick-btn:hover:not(:disabled) {
    color: var(--text);
    border-color: var(--border-hover);
  }
  .pick-btn:disabled { opacity: 0.4; cursor: default; }

  .submit-btn {
    background: var(--accent);
    color: var(--bg);
    font-weight: 600;
    padding: 0.375rem 0.875rem;
    border-radius: var(--radius-sm);
    font-size: 0.8125rem;
  }
  .submit-btn:hover:not(:disabled) { background: var(--accent-hover); }
  .submit-btn:disabled { opacity: 0.4; cursor: default; }

  .picker {
    border-top: 1px solid var(--border);
    padding-top: 0.5rem;
    font-size: 0.8125rem;
    max-height: 18rem;
    overflow: auto;
  }

  .pr-options {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.125rem;
  }

  .pr-option {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    text-align: left;
    padding: 0.3125rem 0.5rem;
    border-radius: var(--radius-sm);
  }
  .pr-option:hover:not(:disabled) { background: rgba(255, 255, 255, 0.05); }
  .pr-option:disabled { opacity: 0.5; cursor: default; }

  .pr-num {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    color: var(--accent);
  }
  .pr-title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .load-error {
    background: rgba(238, 85, 68, 0.1);
    border: 1px solid rgba(238, 85, 68, 0.25);
    color: var(--bad);
    border-radius: var(--radius-sm);
    padding: 0.5rem 0.75rem;
    font-size: 0.8125rem;
  }

  /* ── Empty ───────────────────────────────────────────────────── */
  .empty-state {
    text-align: center;
    padding: 3rem 1rem;
    color: var(--text-dim);
  }
  .empty-icon {
    font-size: 2rem;
    opacity: 0.25;
    margin-bottom: 0.75rem;
  }
  .empty-title {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text);
    margin-bottom: 0.25rem;
  }
  .empty-sub {
    font-size: 0.8125rem;
    max-width: 28rem;
    margin: 0 auto;
    line-height: 1.5;
  }

  /* ── Review list ─────────────────────────────────────────────── */
  .review-list {
    display: flex;
    flex-direction: column;
    gap: 0.625rem;
  }

  .review-card {
    background: var(--bg-card);
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    overflow: hidden;
    transition: border-color var(--transition), box-shadow var(--transition);
  }
  .review-card:hover {
    border-color: var(--border-hover);
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15);
  }

  .review-header {
    padding: 0.75rem 1rem;
    display: flex;
    align-items: flex-start;
    gap: 0.75rem;
  }

  .review-info {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 0.1875rem;
  }

  .name-row {
    display: flex;
    align-items: baseline;
    gap: 0.4375rem;
    min-width: 0;
  }

  .repo-name {
    font-weight: 600;
    white-space: nowrap;
  }

  .pr-link {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    font-size: 0.8125rem;
  }

  .title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .meta {
    font-size: 0.6875rem;
    color: var(--text-dim);
  }
  .meta-sep { margin: 0 0.25rem; opacity: 0.4; }

  .badge-row {
    display: flex;
    gap: 0.375rem;
    flex-wrap: wrap;
    margin-top: 0.125rem;
  }

  .badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    background: rgba(136, 136, 136, 0.12);
    color: var(--text-dim);
    white-space: nowrap;
  }
  .badge.accent, .badge.vouched {
    background: rgba(88, 166, 255, 0.12);
    color: var(--accent);
  }
  .badge.running {
    background: rgba(68, 238, 136, 0.1);
    color: var(--ok);
  }
  .badge.state-open { background: rgba(68, 238, 136, 0.1); color: var(--ok); }
  .badge.state-merged { background: rgba(167, 139, 250, 0.12); color: #c4b5fd; }
  .badge.state-closed { color: var(--stale); }
  .badge.held, .badge.verdict-failed {
    background: rgba(238, 85, 68, 0.12);
    color: var(--bad);
  }
  .badge.failing, .badge.verdict-partial {
    background: rgba(255, 153, 0, 0.12);
    color: var(--sev-medium);
  }
  .badge.verdict-ok { background: rgba(68, 238, 136, 0.1); color: var(--ok); }
  .badge.post-pending {
    background: rgba(255, 153, 0, 0.12);
    color: var(--sev-medium);
  }

  .discard-btn {
    font-size: 0.6875rem;
    padding: 0.1875rem 0.625rem;
    border-radius: var(--radius-sm);
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .discard-btn:hover:not(:disabled) { color: var(--bad); background: rgba(238, 85, 68, 0.1); }
  .discard-btn:disabled { opacity: 0.4; cursor: default; }

  .review-actions {
    display: flex;
    gap: 0.375rem;
    flex-shrink: 0;
  }

  .toggle-btn {
    font-size: 0.6875rem;
    padding: 0.1875rem 0.625rem;
    border-radius: var(--radius-sm);
    font-weight: 500;
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .toggle-btn.is-on {
    color: var(--ok);
    background: rgba(68, 238, 136, 0.08);
  }
  .toggle-btn:hover:not(:disabled) { background: rgba(68, 238, 136, 0.18); }
  .toggle-btn:disabled { opacity: 0.4; cursor: default; }

  .failure-panel {
    border-top: 1px solid var(--border);
    padding: 0.5rem 1rem;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    font-size: 0.75rem;
    color: var(--sev-medium);
    background: rgba(255, 153, 0, 0.05);
  }
  .failure-panel.held {
    color: var(--bad);
    background: rgba(238, 85, 68, 0.06);
  }
  .failure-text {
    flex: 1;
    min-width: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .retry-btn {
    font-size: 0.6875rem;
    padding: 0.1875rem 0.625rem;
    border-radius: var(--radius-sm);
    color: var(--accent);
    background: rgba(88, 166, 255, 0.1);
    flex-shrink: 0;
  }
  .retry-btn:hover:not(:disabled) { background: rgba(88, 166, 255, 0.2); }
  .retry-btn:disabled { opacity: 0.4; cursor: default; }

  /* ── Rounds ──────────────────────────────────────────────────── */
  .rounds {
    border-top: 1px solid var(--border);
    background: var(--bg-panel);
    padding: 0.625rem 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.625rem;
  }

  .round {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }

  .round-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.75rem;
    flex-wrap: wrap;
  }

  .range { color: var(--text); }
  .range.replies-round { color: var(--accent); }

  .issue-count { color: var(--text-dim); }
  .issue-count.has-blocking { color: var(--bad); }
  .resolution-count { color: var(--ok); }
  .answer-count { color: var(--accent); }

  .round-failure {
    margin: 0;
    font-size: 0.75rem;
    color: var(--bad);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .issues {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }

  .issue {
    background: var(--bg);
    border: 1px solid var(--border);
    border-left: 3px solid var(--border-hover);
    border-radius: var(--radius-sm);
    padding: 0.375rem 0.625rem;
    font-size: 0.8125rem;
  }
  .issue.blocking { border-left-color: var(--bad); }
  /* Settled issues stay listed for the record, but recede. */
  .issue.settled { border-left-color: var(--ok); opacity: 0.75; }

  .issue-head {
    display: flex;
    align-items: center;
    gap: 0.375rem;
    flex-wrap: wrap;
    margin-bottom: 0.1875rem;
  }

  .chip {
    font-size: 0.625rem;
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    background: rgba(136, 136, 136, 0.12);
    color: var(--text-dim);
    text-transform: uppercase;
    font-weight: 600;
  }
  .chip.chip-blocking {
    background: rgba(238, 85, 68, 0.12);
    color: var(--bad);
  }
  .chip.sev-high { color: var(--sev-high); }
  .chip.sev-medium { color: var(--sev-medium); }
  .chip.sev-low { color: var(--sev-low); }
  .chip.chip-inline {
    background: rgba(88, 166, 255, 0.12);
    color: var(--accent);
  }
  .chip.status-addressed, .chip.chip-resolved {
    background: rgba(68, 238, 136, 0.1);
    color: var(--ok);
  }
  .chip.status-rebutted {
    background: rgba(167, 139, 250, 0.12);
    color: #c4b5fd;
  }
  .chip.status-reopened {
    background: rgba(238, 85, 68, 0.12);
    color: var(--sev-medium);
  }

  .location {
    font-size: 0.6875rem;
    color: var(--text-dim);
    overflow-wrap: anywhere;
  }

  .issue-summary { margin: 0; }

  .verdict-note {
    margin: 0.25rem 0 0;
    font-size: 0.75rem;
    color: var(--text-dim);
    font-style: italic;
  }

  .replies {
    list-style: none;
    margin: 0.375rem 0 0;
    padding: 0 0 0 0.625rem;
    border-left: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: 0.75rem;
  }
  .reply-author {
    font-weight: 600;
    color: var(--text-dim);
  }
  .reply.own .reply-author { color: var(--accent); }
  .reply-body {
    margin: 0.0625rem 0 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .issue-detail summary { cursor: pointer; }
  .issue-detail pre {
    margin: 0.375rem 0 0;
    font-size: 0.75rem;
    color: var(--text-dim);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    line-height: 1.5;
  }

  .earlier > summary {
    cursor: pointer;
    font-size: 0.75rem;
    color: var(--text-dim);
  }
  .earlier .round { margin-top: 0.625rem; }

  @media (max-width: 600px) {
    .review-header { flex-direction: column; }
  }
</style>
