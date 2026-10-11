<script lang="ts">
  import { store, post } from "../lib/api.svelte";
  import type { FindingOut } from "../lib/types";
  import { isHttpUrl, pct, datetime, typeLabel, holdReasonLabel, releaseActionLabel } from "../lib/format";
  import { nearViewport } from "../lib/nearViewport";
  import { navigation } from "../lib/navigation.svelte";
  import FindingDetail from "./FindingDetail.svelte";

  let {
    finding,
    actions = false,
    focused = false,
  }: { finding: FindingOut; actions?: boolean; focused?: boolean } = $props();

  let expanded = $state(false);
  // Absent when talking to a daemon that predates the field.
  let followUps = $derived(finding.follow_ups ?? []);
  let reasonText = $state("");
  let showReasonPrompt = $state<string | null>(null); // "rejected" | "wontfix" | null
  // Held work (WORK-MODEL.md §9): absent when talking to a daemon that
  // predates the field. One hold per kind, so the kind keys a prompt.
  let holds = $derived(finding.holds ?? []);
  let holdPrompt = $state<{ kind: string; action: string } | null>(null);
  let holdReason = $state("");
  let classification = $state("superseded");
  let busy = $state(false);
  let timelineOpen = $state(false);

  // Only cards on or near screen are mounted: the findings list runs to
  // thousands of cards, and mounting all of them — then re-rendering all of
  // them on every 5s poll — froze the page for seconds. An off-screen card
  // keeps its place with an empty box of its last rendered height; until it
  // has rendered once, the list's --card-estimate (see .placeholder).
  //
  // The box is a child, never a height on the card itself: the browser's
  // scroll anchoring holds a visible card in place while cards above it
  // change size, but a change to the anchor's own `height` switches that
  // off for the frame. After any jump into unrendered cards (a scrollbar
  // drag, a deep link) the anchor is a placeholder about to mount, so the
  // content would slide by everything that mounted above it.
  let near = $state(false);
  let lastHeight = $state<number | null>(null);
  // Mid-interaction cards stay mounted off screen: unmounting would close an
  // open panel or drop a half-typed reason.
  const live = $derived(
    near || expanded || timelineOpen || showReasonPrompt !== null || holdPrompt !== null || busy,
  );

  // Which actions the finding's status allows, mirroring the server's state
  // machine (`FindingStatus::awaits_verdict` and the /api/recheck and
  // /api/unqueue preconditions), so a card never offers a button the API
  // would refuse. `actions` only says whether the page shows them at all.
  const awaitsVerdict = $derived(["new", "note", "closed"].includes(finding.status));
  const canRecheck = $derived(finding.status === "new");
  const canUnqueue = $derived(finding.status === "queued");
  // An override is honoured by the tiers that pick queued, rechecking,
  // pr_open and closed/merged-awaiting-harvest findings, and can be set
  // ahead on one still waiting for the operator. A running fix, or a
  // finding nothing will pick again, has no use for one; clearing an
  // existing one is always allowed.
  const canOverride = $derived(
    !["fixing", "rejected", "wontfix", "superseded"].includes(finding.status),
  );
  const hasActions = $derived(
    actions && (awaitsVerdict || canRecheck || canUnqueue || canOverride || !!finding.budget_override),
  );

  function onVisibility(entry: IntersectionObserverEntry) {
    if (entry.isIntersecting) {
      near = true;
      return;
    }
    if (near) lastHeight = entry.boundingClientRect.height;
    near = false;
  }

  // "PR #7" where the URL ends in a number, both forges' shape
  // (/pull/7, /merge_requests/7). Anything else keeps the generic label
  // rather than showing a wrong number -- the full URL is the tooltip.
  function prLabel(url: string): string {
    const n = /\/(\d+)\/?$/.exec(url);
    return n ? `PR #${n[1]}` : "PR";
  }

  function statusBadgeClass(status: string): string {
    switch (status) {
      case "new": return "status-new";
      case "queued": return "status-queued";
      case "rechecking": return "status-rechecking";
      case "fixing": return "status-fixing";
      case "pr_open": return "status-pr-open";
      case "merged": return "status-merged";
      case "closed": return "status-closed";
      case "superseded": return "status-superseded";
      case "rejected": return "status-rejected";
      case "wontfix": return "status-wontfix";
      case "note": return "status-note";
      default: return "status-default";
    }
  }

  function sevClass(sev: string): string {
    switch (sev) {
      case "high": return "sev-high";
      case "medium": return "sev-medium";
      case "low": return "sev-low";
      default: return "sev-default";
    }
  }

  async function doVerdict(status: string, reason?: string) {
    busy = true;
    try {
      const r = await post("/api/verdict", { id: finding.id, status, reason: reason || undefined });
      if (!r.ok) {
        // post() resolves on 4xx/5xx — keep the reason prompt and its text.
        console.error(`verdict ${status} for finding ${finding.id} rejected with HTTP ${r.status}`);
        return;
      }
      await store.refresh();
      showReasonPrompt = null;
      reasonText = "";
    } catch (err) {
      console.error(`verdict ${status} for finding ${finding.id} failed`, err);
    } finally {
      // Re-arm the buttons even if the request never reached the server.
      busy = false;
    }
  }

  // A verdict needs the operator's reason, and a closed PR's
  // classification its class: those actions ask first.
  const needsReason = (action: string) => ["reject", "wontfix", "classify"].includes(action);

  async function doRelease(kind: string, action: string, reason?: string) {
    busy = true;
    try {
      const r = await post("/api/hold/release", {
        finding_id: finding.id,
        kind,
        action,
        reason: reason || undefined,
        classification: action === "classify" ? classification : undefined,
      });
      if (!r.ok) {
        // post() resolves on 4xx/5xx — keep the prompt and its text.
        console.error(`release ${action} of finding ${finding.id}'s ${kind} rejected with HTTP ${r.status}`);
        return;
      }
      await store.refresh();
      holdPrompt = null;
      holdReason = "";
    } catch (err) {
      console.error(`release ${action} of finding ${finding.id}'s ${kind} failed`, err);
    } finally {
      busy = false;
    }
  }

  async function doRecheck() {
    busy = true;
    try {
      const r = await post("/api/recheck", { id: finding.id });
      if (!r.ok) {
        console.error(`recheck finding ${finding.id} rejected with HTTP ${r.status}`);
        return;
      }
      await store.refresh();
    } catch (err) {
      console.error(`recheck finding ${finding.id} failed`, err);
    } finally {
      busy = false;
    }
  }

  async function doUnqueue() {
    busy = true;
    try {
      const r = await post("/api/unqueue", { id: finding.id });
      if (!r.ok) {
        console.error(`unqueue finding ${finding.id} rejected with HTTP ${r.status}`);
        return;
      }
      await store.refresh();
    } catch (err) {
      console.error(`unqueue finding ${finding.id} failed`, err);
    } finally {
      busy = false;
    }
  }

  async function doOverride(mode: string | null) {
    busy = true;
    try {
      const r = await post("/api/override", { id: finding.id, mode });
      if (!r.ok) {
        console.error(`override ${mode} for finding ${finding.id} rejected with HTTP ${r.status}`);
        return;
      }
      await store.refresh();
    } catch (err) {
      console.error(`override ${mode} for finding ${finding.id} failed`, err);
    } finally {
      busy = false;
    }
  }
</script>

<div
  class="card {sevClass(finding.severity)}"
  class:focused
  id="finding-{finding.id}"
  {@attach nearViewport(onVisibility)}
>
  {#if !live}
    <div class="placeholder" style:--placeholder-height={lastHeight == null ? null : `${lastHeight}px`}></div>
  {:else}
    <!-- Header row: id + type pill + severity dot/text + confidence + category + status pill (right) -->
    <div class="card-header">
      <a class="fid" href={navigation.findingHref(finding.id)} title="Open finding #{finding.id}">F#{finding.id}</a>
      <span class="type-pill" title={finding.type}>{typeLabel(finding.type)}</span>
      <span class="sev-indicator {sevClass(finding.severity)}">
        <span class="sev-dot"></span>
        {finding.severity}
      </span>
      <span class="confidence">{pct(finding.confidence)}</span>
      {#if finding.category}
        <span class="cat-label">{finding.category}</span>
      {/if}
      {#if finding.budget_override}
        <span class="override-badge" title="Budget override">⚡ {finding.budget_override}</span>
      {/if}
      {#if finding.needs_attention}
        <span class="attn-badge" title="Needs attention">⚠ {finding.needs_attention}</span>
      {/if}
      <!-- In the header, not just in the detail panel: once a finding has a
           PR, that PR is the thing you want -- open ones to review, merged
           ones to see what shipped -- and needing to expand the card made
           the most common next step the least reachable.
           Only linked when it parses as http(s) -- the URL is scraped from
           `gh`/`glab` stdout, so the detail panel still shows anything else
           as plain text rather than making it clickable. -->
      {#if finding.pr_url && isHttpUrl(finding.pr_url)}
        <a
          class="pr-chip"
          class:pr-chip-live={finding.status === "pr_open" || finding.status === "merged"}
          href={finding.pr_url}
          target="_blank"
          rel="noopener"
          title={finding.pr_url}
        >
          {prLabel(finding.pr_url)} ↗
        </a>
      {/if}
      <span class="status-pill {statusBadgeClass(finding.status)}">
        {finding.status}
      </span>
    </div>

    <!-- Summary -->
    <p class="summary">{finding.summary}</p>

    {#each holds as hold (hold.kind)}
      <div class="hold">
        <div class="hold-head">
          <strong>Needs you:</strong> {hold.kind} — {holdReasonLabel(hold.reason)}
          <span class="hold-at">since {datetime(hold.held_at)}</span>
          {#if hold.new_activity}
            <span class="attn-badge" title="The PR has had activity since this was held">new activity since hold</span>
          {/if}
        </div>
        <div class="hold-detail">{hold.detail || "(no detail recorded)"}</div>
        {#if actions}
          {#if holdPrompt?.kind === hold.kind}
            <div class="reason-row">
              {#if holdPrompt.action === "classify"}
                <select class="reason-input" bind:value={classification} aria-label="Classification">
                  {#each ["superseded", "duplicate", "obsolete", "wrong", "unwanted", "abandoned"] as c (c)}
                    <option value={c}>{c}</option>
                  {/each}
                </select>
              {/if}
              <input
                type="text"
                class="reason-input"
                placeholder="Reason for {releaseActionLabel(holdPrompt.action)}…"
                bind:value={holdReason}
                onkeydown={(e: KeyboardEvent) => { if (e.key === "Enter" && !busy && holdReason.trim()) doRelease(hold.kind, holdPrompt!.action, holdReason.trim()); }}
              />
              <button
                class="btn btn-confirm"
                disabled={busy || !holdReason.trim()}
                onclick={() => doRelease(hold.kind, holdPrompt!.action, holdReason.trim())}
              >Confirm</button>
              <button class="btn btn-muted" onclick={() => { holdPrompt = null; holdReason = ""; }}>Cancel</button>
            </div>
          {:else}
            <div class="btn-row">
              {#each hold.actions as action (action)}
                <button
                  class="btn {action === 'continue' ? 'btn-queue' : action === 'reject' ? 'btn-reject' : 'btn-muted'}"
                  disabled={busy}
                  onclick={() => { if (needsReason(action)) { holdPrompt = { kind: hold.kind, action }; } else { doRelease(hold.kind, action); } }}
                >{releaseActionLabel(action)}</button>
              {/each}
            </div>
          {/if}
        {/if}
      </div>
    {/each}

    <!-- Subtitle: fingerprint + file:line -->
    <div class="meta-line">
      <span class="fingerprint" title={finding.fingerprint}>{finding.fingerprint}</span>
      {#if finding.file}
        <span class="file-loc">
          {finding.file}{#if finding.line}:{finding.line}{/if}
        </span>
      {/if}
    </div>

    <!-- Lineage: an engage/harvest job that files FOLLOW-UPS.json leaves the
         new findings under new fingerprints, so this is the only path between
         a withdrawn finding and what replaced it. -->
    {#if finding.follow_up_of != null || followUps.length > 0}
      <div class="lineage">
        {#if finding.follow_up_of != null}
          <span>follow-up of <a href={navigation.findingHref(finding.follow_up_of)}>F#{finding.follow_up_of}</a></span>
        {/if}
        {#if followUps.length > 0}
          <span>
            follow-ups:
            {#each followUps as fid (fid)}
              <a href={navigation.findingHref(fid)}>F#{fid}</a>
            {/each}
          </span>
        {/if}
      </div>
    {/if}

    <!-- Timeline (collapsible) -->
    {#if finding.timeline && finding.timeline.length > 0}
      <details class="timeline" bind:open={timelineOpen}>
        <summary class="timeline-toggle">
          Timeline ({finding.timeline.length})
        </summary>
        <!-- Rows are built on open only: a closed <details> hides its
             content but Svelte still creates and updates all of it. -->
        {#if timelineOpen}
          <div class="timeline-items">
            {#each finding.timeline as ev (ev.id)}
              <div class="tl-row">
                <span class="tl-dot"></span>
                <span class="tl-time">{datetime(ev.at)}</span>
                <span class="tl-kind">{ev.kind}</span>
                {#if ev.username}<span class="tl-by">by {ev.username}</span>{/if}
                <span class="tl-msg">{ev.message}</span>
              </div>
            {/each}
          </div>
        {/if}
      </details>
    {/if}

    <!-- Actions (verdict / recheck / override) -->
    {#if hasActions}
      <div class="actions">
        {#if showReasonPrompt}
          <!-- Reason input for rejected/wontfix -->
          <div class="reason-row">
            <input
              type="text"
              class="reason-input"
              placeholder="Reason for {showReasonPrompt}…"
              bind:value={reasonText}
              onkeydown={(e: KeyboardEvent) => { if (e.key === "Enter" && !busy && reasonText.trim()) doVerdict(showReasonPrompt!, reasonText.trim()); }}
            />
            <button
              class="btn btn-confirm"
              disabled={busy || !reasonText.trim()}
              onclick={() => doVerdict(showReasonPrompt!, reasonText.trim())}
            >Confirm</button>
            <button
              class="btn btn-muted"
              onclick={() => { showReasonPrompt = null; reasonText = ""; }}
            >Cancel</button>
          </div>
        {:else}
          <div class="btn-row">
            {#if awaitsVerdict}
              <button
                class="btn btn-queue"
                disabled={busy}
                onclick={() => doVerdict("queued")}
                title="Queue for fix"
              >Queue</button>
              <button class="btn btn-reject" disabled={busy} onclick={() => { showReasonPrompt = "rejected"; }} title="Reject">Reject</button>
              <button class="btn btn-muted" disabled={busy} onclick={() => { showReasonPrompt = "wontfix"; }} title="Won't fix">Wontfix</button>
              {#if finding.status !== "note"}
                <button class="btn btn-muted" disabled={busy} onclick={() => doVerdict("note")} title="Mark as note">Note</button>
              {/if}
            {/if}

            {#if canRecheck}
              <span class="sep"></span>
              <button class="btn btn-accent" disabled={busy} onclick={doRecheck} title="Queue for adversarial recheck">Recheck</button>
            {/if}

            {#if canUnqueue}
              <button class="btn btn-warn" disabled={busy} onclick={doUnqueue} title="Remove from fix queue">Unqueue</button>
            {/if}

            {#if finding.budget_override || canOverride}
              {#if awaitsVerdict || canUnqueue}<span class="sep"></span>{/if}
              {#if finding.budget_override}
                <button class="btn btn-muted" disabled={busy} onclick={() => doOverride(null)} title="Clear budget override">Clear override</button>
              {:else}
                <button class="btn btn-accent" disabled={busy} onclick={() => doOverride("once")} title="Budget override: once">⚡ Once</button>
                <button class="btn btn-accent" disabled={busy} onclick={() => doOverride("exempt")} title="Budget override: exempt">⚡ Exempt</button>
              {/if}
            {/if}
          </div>
        {/if}
      </div>
    {/if}

    <!-- Expandable detail panel -->
    <button
      class="detail-toggle"
      onclick={() => { expanded = !expanded; }}
    >
      {expanded ? "▾ Hide detail" : "▸ Show detail"}
    </button>
    {#if expanded}
      <div class="detail-panel">
        {#if finding.detail}
          <p class="detail-text">{finding.detail}</p>
        {/if}
        {#if finding.evidence_plan}
          <div class="detail-section">
            <span class="detail-label">Evidence plan:</span>
            <p class="detail-text" style="margin-top:0.125rem">{finding.evidence_plan}</p>
          </div>
        {/if}
        {#if finding.pr_url}
          <div class="detail-section">
            <span class="detail-label">PR:</span>
            {#if isHttpUrl(finding.pr_url)}
              <a href={finding.pr_url} target="_blank" rel="noopener" class="detail-link"
                >{finding.pr_url}</a
              >
            {:else}
              <!-- Parsed out of `gh`/`glab` stdout, so not ours to trust as a
                   navigable URL. Shown, not linked. -->
              <span class="detail-link">{finding.pr_url}</span>
            {/if}
          </div>
        {/if}
        {#if finding.verdict_reason}
          <div class="detail-section">
            <span class="detail-label">Verdict reason:</span>
            <span class="detail-value">{finding.verdict_reason}</span>
          </div>
        {/if}
        <FindingDetail findingId={finding.id} />
      </div>
    {/if}
  {/if}
</div>

<style>
  /* ── Card ────────────────────────────────────────────────────── */
  .card {
    /* Shared with .placeholder, which subtracts them from the card height. */
    --card-pad-block: 0.75rem;
    --card-border: 1px;
    background: var(--bg-card);
    border-radius: var(--radius-md);
    padding: var(--card-pad-block) 0.875rem;
    margin-bottom: 0.5rem;
    border: var(--card-border) solid var(--border);
    border-left: 3px solid var(--text-dim);
    transition: transform var(--transition), box-shadow var(--transition),
                border-color var(--transition);
    overflow: hidden;
    min-width: 0;
  }
  /* --card-estimate is set on the list when restoring a scroll position
     (lib/scrollMemory.ts), from the real height of the cards above it. */
  .placeholder {
    height: calc(
      var(--placeholder-height, var(--card-estimate, 180px)) - 2 * var(--card-pad-block) - 2 * var(--card-border)
    );
  }
  .card:hover {
    transform: translateY(-1px);
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25);
    border-color: var(--border-hover);
  }
  .card.sev-high   { border-left-color: var(--sev-high); }
  .card.sev-medium { border-left-color: var(--sev-medium); }
  .card.sev-low    { border-left-color: var(--sev-low); }
  /* The finding a deep link points at. */
  .card.focused {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    box-shadow: 0 0 12px rgba(78, 168, 222, 0.3), inset 0 0 0 1px rgba(78, 168, 222, 0.15);
  }

  /* ── Header ─────────────────────────────────────────────────── */
  .card-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    font-size: 0.75rem;
  }

  .type-pill {
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    font-size: 0.6875rem;
    font-weight: 500;
    background: rgba(255, 255, 255, 0.06);
    color: var(--text-dim);
  }

  .sev-indicator {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    font-size: 0.6875rem;
    font-weight: 600;
    text-transform: uppercase;
  }
  .sev-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--text-dim);
  }
  .sev-indicator.sev-high   { color: var(--sev-high); }
  .sev-indicator.sev-high .sev-dot { background: var(--sev-high); }
  .sev-indicator.sev-medium { color: var(--sev-medium); }
  .sev-indicator.sev-medium .sev-dot { background: var(--sev-medium); }
  .sev-indicator.sev-low    { color: var(--sev-low); }
  .sev-indicator.sev-low .sev-dot { background: var(--sev-low); }

  .confidence {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }
  .fid {
    color: var(--text-dim);
    font-size: 0.6875rem;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    user-select: all;
  }
  .fid:hover, .fid:focus-visible {
    color: var(--accent-hover);
    text-decoration: underline;
  }

  .cat-label {
    font-size: 0.6875rem;
    color: rgba(136, 136, 136, 0.7);
  }

  .override-badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.3125rem;
    border-radius: 99px;
    background: rgba(78, 168, 222, 0.12);
    color: var(--accent);
  }

  .attn-badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.3125rem;
    border-radius: 99px;
    background: rgba(238, 85, 68, 0.12);
    color: var(--sev-high);
  }

  /* Muted only where the PR led nowhere -- a rejected finding's PR is a
     dead end. An open PR is waiting on you and a merged one is the
     record of what actually shipped; both stay prominent. */
  .pr-chip {
    font-size: 0.625rem;
    font-weight: 600;
    padding: 0.05rem 0.4rem;
    border-radius: 99px;
    border: 1px solid var(--border);
    color: var(--text-dim);
    text-decoration: none;
    white-space: nowrap;
  }
  .pr-chip:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  .pr-chip-live {
    border-color: var(--accent);
    color: var(--accent);
  }
  .pr-chip-live:hover {
    background: var(--accent);
    color: var(--bg);
  }

  .status-pill {
    margin-left: auto;
    padding: 0.0625rem 0.4375rem;
    border-radius: 99px;
    font-size: 0.625rem;
    font-weight: 500;
    letter-spacing: 0.02em;
  }
  .status-new, .status-rechecking { background: rgba(78, 168, 222, 0.12); color: var(--accent); }
  .status-queued, .status-fixing  { background: rgba(255, 153, 0, 0.12); color: var(--sev-medium); }
  .status-pr-open                 { background: rgba(68, 238, 136, 0.12); color: var(--ok); }
  .status-merged                  { background: rgba(68, 238, 136, 0.18); color: var(--ok); }
  /* Closed unmerged, awaiting its harvest: not a failure (and not suppressed),
     so amber like the other in-flight states rather than red. */
  .status-closed                  { background: rgba(255, 153, 0, 0.08); color: var(--sev-medium); }
  .status-superseded              { background: rgba(68, 238, 136, 0.08); color: var(--ok); }
  .status-rejected                { background: rgba(238, 85, 68, 0.12); color: var(--bad); }
  .status-wontfix, .status-note   { background: rgba(136, 136, 136, 0.12); color: var(--text-dim); }
  .status-default                 { background: rgba(255, 255, 255, 0.06); color: var(--text-dim); }

  /* ── Summary ────────────────────────────────────────────────── */
  .summary {
    margin: 0.375rem 0 0;
    font-size: 0.9375rem;
    line-height: 1.5;
    overflow-wrap: break-word;
    word-break: break-word;
  }

  .hold {
    margin-top: 0.5rem;
    padding: 0.5rem 0.625rem;
    border-left: 2px solid var(--sev-medium);
    background: var(--bg-panel);
    font-size: 0.75rem;
  }
  .hold-at {
    color: var(--text-dim);
    margin-left: 0.375rem;
  }
  .hold-detail {
    margin: 0.25rem 0 0.375rem;
    white-space: pre-wrap;
    max-height: 8rem;
    overflow: auto;
    color: var(--text-dim);
  }

  /* ── Meta (fingerprint + location) ──────────────────────────── */
  .meta-line {
    margin-top: 0.25rem;
    display: flex;
    gap: 0.75rem;
    font-size: 0.6875rem;
    color: var(--text-dim);
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    min-width: 0;
    overflow: hidden;
  }
  .fingerprint {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .file-loc {
    flex-shrink: 0;
    color: rgba(136, 136, 136, 0.7);
  }
  .lineage {
    margin-top: 0.25rem;
    display: flex;
    flex-wrap: wrap;
    gap: 0.75rem;
    font-size: 0.6875rem;
    color: var(--text-dim);
  }
  .lineage a {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    color: var(--accent);
    text-decoration: none;
    margin-left: 0.25rem;
  }
  .lineage a:hover {
    text-decoration: underline;
  }

  /* ── Timeline ───────────────────────────────────────────────── */
  .timeline {
    margin-top: 0.5rem;
  }

  .timeline-toggle {
    font-size: 0.6875rem;
    color: var(--text-dim);
    cursor: pointer;
    user-select: none;
  }
  .timeline-toggle:hover {
    color: var(--text);
  }

  .timeline-items {
    margin-top: 0.375rem;
    padding-left: 0.375rem;
    border-left: 1px solid rgba(255, 255, 255, 0.08);
    font-size: 0.6875rem;
    max-height: 10rem;
    overflow-y: auto;
  }

  .tl-row {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    color: var(--text-dim);
    padding: 0.1875rem 0;
    position: relative;
  }

  .tl-dot {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.2);
    flex-shrink: 0;
    position: relative;
    left: -0.625rem;
    margin-right: -0.375rem;
  }

  .tl-time {
    flex-shrink: 0;
    width: 7rem;
  }

  .tl-kind {
    font-weight: 600;
    color: var(--text);
    width: 4rem;
    flex-shrink: 0;
  }

  .tl-by {
    flex-shrink: 0;
    white-space: nowrap;
  }

  .tl-msg {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* ── Actions bar ────────────────────────────────────────────── */
  .actions {
    margin-top: 0.5rem;
    padding-top: 0.5rem;
    border-top: 1px solid var(--border);
  }

  .reason-row {
    display: flex;
    gap: 0.375rem;
    align-items: center;
  }

  .reason-input {
    flex: 1;
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 0.25rem 0.5rem;
    font-size: 0.75rem;
    color: var(--text);
  }
  .reason-input::placeholder {
    color: var(--text-dim);
  }
  .reason-input:focus {
    outline: none;
    border-color: var(--accent);
  }

  .btn-row {
    display: flex;
    gap: 0.3125rem;
    flex-wrap: wrap;
  }

  .btn {
    padding: 0.1875rem 0.4375rem;
    font-size: 0.6875rem;
    border-radius: var(--radius-sm);
    font-weight: 500;
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .btn:disabled { opacity: 0.4; }

  .btn-queue { color: var(--text-dim); }
  .btn-queue:hover:not(:disabled) { background: rgba(68, 238, 136, 0.15); color: var(--ok); }

  .btn-reject { color: var(--text-dim); }
  .btn-reject:hover:not(:disabled) { background: rgba(238, 85, 68, 0.15); color: var(--bad); }

  .btn-muted:hover:not(:disabled) { background: rgba(255, 255, 255, 0.1); color: var(--text); }

  .btn-accent { color: var(--text-dim); }
  .btn-accent:hover:not(:disabled) { background: rgba(78, 168, 222, 0.15); color: var(--accent); }

  .btn-warn { color: var(--text-dim); }
  .btn-warn:hover:not(:disabled) { background: rgba(255, 153, 0, 0.15); color: var(--sev-medium); }

  .btn-confirm { background: rgba(238, 85, 68, 0.15); color: var(--bad); }
  .btn-confirm:hover:not(:disabled) { background: rgba(238, 85, 68, 0.25); }

  .sep {
    border-left: 1px solid var(--border);
    margin: 0 0.0625rem;
    align-self: stretch;
  }

  /* ── Detail toggle ──────────────────────────────────────────── */
  .detail-toggle {
    margin-top: 0.5rem;
    font-size: 0.6875rem;
    color: var(--accent);
    cursor: pointer;
    user-select: none;
    background: none;
    border: none;
    padding: 0;
  }
  .detail-toggle:hover {
    color: var(--accent-hover);
  }

  /* ── Detail panel (card-in-card) ────────────────────────────── */
  .detail-panel {
    margin-top: 0.375rem;
    background: var(--bg-panel);
    border-radius: var(--radius-md);
    padding: 0.625rem 0.75rem;
    border: 1px solid var(--border);
    box-shadow: inset 0 1px 3px rgba(0, 0, 0, 0.2);
    animation: fadeIn 150ms ease both;
  }

  .detail-text {
    font-size: 0.75rem;
    color: var(--text-dim);
    white-space: pre-wrap;
    margin-bottom: 0.5rem;
    line-height: 1.5;
  }

  .detail-section {
    font-size: 0.75rem;
    margin-bottom: 0.5rem;
  }

  .detail-label {
    color: var(--text-dim);
    font-weight: 600;
  }

  .detail-link {
    color: var(--accent);
    margin-left: 0.25rem;
  }
  .detail-link:hover {
    color: var(--accent-hover);
  }

  .detail-value {
    color: var(--text-dim);
    margin-left: 0.25rem;
  }
</style>
