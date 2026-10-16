<script lang="ts">
  import { onMount, onDestroy, untrack } from "svelte";
  import { store } from "./lib/api.svelte";
  import { navigation } from "./lib/navigation.svelte";
  import * as scrollMemory from "./lib/scrollMemory";
  import StatusPage from "./pages/StatusPage.svelte";
  import InboxPage from "./pages/InboxPage.svelte";
  import KanbanPage from "./pages/KanbanPage.svelte";
  import AllFindingsPage from "./pages/AllFindingsPage.svelte";
  import ReposPage from "./pages/ReposPage.svelte";
  import StatsPage from "./pages/StatsPage.svelte";
  import LogPage from "./pages/LogPage.svelte";
  import Login from "./components/Login.svelte";
  import Toasts from "./components/Toasts.svelte";
  import "./app.css";

  const NAV = [
    { id: "status", label: "STATUS", icon: "◈" },
    { id: "inbox", label: "INBOX", icon: "▣" },
    { id: "kanban", label: "KANBAN", icon: "▥" },
    { id: "findings", label: "FINDINGS", icon: "◉" },
    { id: "repos", label: "REPOS", icon: "⌂" },
    { id: "stats", label: "STATS", icon: "▤" },
    { id: "log", label: "LOG", icon: "≡" },
  ] as const;

  const page = $derived(navigation.route.page);
  const focusId = $derived(navigation.route.focusId);
  let sidebarOpen = $state(false);

  function navigate(id: string) {
    navigation.navigate(id);
    sidebarOpen = false;
  }

  function onHashChange() {
    navigation.sync();
  }

  // Scroll position per history entry. `navigate` fires while the entry
  // being left is still current, and `pagehide` covers reload and close.
  let scroller = $state<HTMLElement | null>(null);

  function saveScroll() {
    if (scroller) scrollMemory.save(scroller);
  }

  // What the entry being shown needs once it has rendered: its saved
  // position, or for a new entry the top. A new entry on another page
  // starts at the top anyway (a fresh scroll container); this covers a
  // filter or sort change, which stays on the page.
  let arrival: { saved: scrollMemory.ScrollState | null; fresh: boolean } = {
    saved: scrollMemory.saved(),
    fresh: false,
  };

  function onEntryChange(event: NavigationCurrentEntryChangeEvent) {
    // updateCurrentEntry() (saving a position) reports itself with no type.
    if (event.navigationType === null) return;
    const traversed = event.navigationType === "traverse" || event.navigationType === "reload";
    arrival = { saved: scrollMemory.saved(), fresh: !traversed };
  }

  let cancelRestore = () => {};
  $effect(() => {
    // Once per entry, after it has rendered. Two entries can share a URL
    // (Back between them changes no hash), and a replace keeps the entry
    // but changes the URL, so follow both.
    void navigation.hash;
    void navigation.entry;
    const el = scroller;
    if (!el) return;
    cancelRestore();
    // Untracked: restore runs its first attempt synchronously, and that
    // reads `store.summary` through `ready`. Tracked, every poll re-ran
    // this effect and pulled the page back to the saved position.
    if (arrival.saved) {
      const saved = arrival.saved;
      cancelRestore = untrack(() => scrollMemory.restore(el, saved, () => store.summary !== null));
    }
    // A deep link places its finding itself (AllFindingsPage).
    else if (arrival.fresh && navigation.route.focusId == null) el.scrollTo({ top: 0, behavior: "instant" });
  });

  let loggingOut = $state(false);
  let logoutError = $state<string | null>(null);

  async function logout() {
    loggingOut = true;
    try {
      logoutError = await store.logout();
    } finally {
      loggingOut = false;
    }
  }

  onMount(() => {
    store.start();
    window.addEventListener("hashchange", onHashChange);
    window.addEventListener("popstate", onHashChange);
    window.addEventListener("pagehide", saveScroll);
    window.navigation.addEventListener("navigate", saveScroll);
    window.navigation.addEventListener("currententrychange", onEntryChange);
  });

  onDestroy(() => {
    store.stopPolling();
    cancelRestore();
    window.removeEventListener("hashchange", onHashChange);
    window.removeEventListener("popstate", onHashChange);
    window.removeEventListener("pagehide", saveScroll);
    window.navigation.removeEventListener("navigate", saveScroll);
    window.navigation.removeEventListener("currententrychange", onEntryChange);
  });
</script>

{#if store.needsLogin}
<Login />
{:else}
<!-- Mobile top bar -->
<header class="topbar">
  <button
    class="hamburger"
    onclick={() => { sidebarOpen = !sidebarOpen; }}
    aria-label="Toggle navigation"
    aria-expanded={sidebarOpen}
    aria-controls="sidebar"
  >
    <svg width="18" height="18" viewBox="0 0 18 18" fill="none">
      <line x1="2" y1="4" x2="16" y2="4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
      <line x1="2" y1="9" x2="16" y2="9" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
      <line x1="2" y1="14" x2="16" y2="14" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
    </svg>
  </button>
  <svg viewBox="0 0 40 40" width="20" height="20" fill="none" xmlns="http://www.w3.org/2000/svg">
    <circle cx="18" cy="18" r="11" stroke="#5aa0e0" stroke-width="2.2" opacity=".85"/>
    <ellipse cx="18" cy="19" rx="5" ry="6.5" fill="#4cbf6b" opacity=".9"/>
    <circle cx="18" cy="11.5" r="2.8" fill="#4cbf6b" opacity=".9"/>
  </svg>
  <span class="topbar-brand">hunter</span>
</header>

<!-- Sidebar overlay backdrop (mobile) -->
{#if sidebarOpen}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="sidebar-backdrop" onclick={() => { sidebarOpen = false; }}></div>
{/if}

<div class="shell">
  <!-- Sidebar nav -->
  <nav class="sidebar" class:open={sidebarOpen} id="sidebar">
    <div class="logo">
      <svg viewBox="0 0 40 40" width="28" height="28" fill="none" xmlns="http://www.w3.org/2000/svg">
        <!-- magnifying glass -->
        <circle cx="18" cy="18" r="11" stroke="#5aa0e0" stroke-width="2.2" opacity=".85"/>
        <line x1="26" y1="26" x2="36" y2="36" stroke="#5aa0e0" stroke-width="2.5" stroke-linecap="round" opacity=".85"/>
        <!-- bug body -->
        <ellipse cx="18" cy="19" rx="5" ry="6.5" fill="#4cbf6b" opacity=".9"/>
        <!-- head -->
        <circle cx="18" cy="11.5" r="2.8" fill="#4cbf6b" opacity=".9"/>
        <!-- legs -->
        <line x1="13" y1="15" x2="9"  y2="12" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="13" y1="19" x2="8"  y2="19" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="13" y1="23" x2="9"  y2="26" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="23" y1="15" x2="27" y2="12" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="23" y1="19" x2="28" y2="19" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="23" y1="23" x2="27" y2="26" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <!-- wing line -->
        <line x1="18" y1="13" x2="18" y2="25" stroke="var(--bg-panel)" stroke-width="1" opacity=".6"/>
        <!-- antennae -->
        <line x1="16" y1="9.5" x2="13" y2="6" stroke="#4cbf6b" stroke-width="1.2" stroke-linecap="round"/>
        <line x1="20" y1="9.5" x2="23" y2="6" stroke="#4cbf6b" stroke-width="1.2" stroke-linecap="round"/>
        <circle cx="13" cy="5.5" r=".9" fill="#4cbf6b"/>
        <circle cx="23" cy="5.5" r=".9" fill="#4cbf6b"/>
      </svg>
      <span class="brand">hunter</span>
    </div>
    <div class="nav-items">
      {#each NAV as { id, label, icon } (id)}
        <button
          class="nav-btn"
          class:active={page === id}
          onclick={() => navigate(id)}
        >
          <span class="nav-icon">{icon}</span>
          {label}
        </button>
      {/each}
    </div>
    <div class="sidebar-user">
      {#if store.user}
        <span class="user-name" title="Signed in as {store.user}">{store.user}</span>
      {/if}
      <button class="logout-btn" onclick={logout} disabled={loggingOut}>
        {loggingOut ? "logging out…" : "log out"}
      </button>
      {#if logoutError}
        <span class="logout-error" role="alert">{logoutError}</span>
      {/if}
    </div>
  </nav>

  <!-- Main content -->
  <main class="content">
    {#if store.error}
      <div class="api-banner" role="status">
        <span class="api-banner-icon">⚠</span>
        <span>Cannot reach the API ({store.error}) — data shown below may be stale.</span>
      </div>
    {/if}
    <!-- Each page owns its scroll container, so a page switch starts at the
         top instead of inheriting the previous page's offset. -->
    {#key page}
      <div class="page-scroll page-enter" bind:this={scroller}>
        {#if page === "status"}
          <StatusPage />
        {:else if page === "inbox"}
          <InboxPage />
        {:else if page === "kanban"}
          <KanbanPage />
        {:else if page === "findings"}
          <AllFindingsPage {focusId} />
        {:else if page === "repos"}
          <ReposPage />
        {:else if page === "stats"}
          <StatsPage />
        {:else if page === "log"}
          <LogPage />
        {:else}
          <InboxPage />
        {/if}
      </div>
    {/key}
  </main>
</div>
<Toasts />
{/if}

<style>
  /* ── Mobile top bar ──────────────────────────────────────────── */
  .topbar {
    display: none;
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    height: 48px;
    background: var(--bg-panel);
    border-bottom: 1px solid var(--border);
    align-items: center;
    padding: 0 0.875rem;
    gap: 0.625rem;
    z-index: 40;
  }

  .hamburger {
    color: var(--text-dim);
    padding: 0.375rem;
    border-radius: var(--radius-sm);
    display: flex;
    align-items: center;
  }
  .hamburger:hover {
    color: var(--text);
    background: rgba(255, 255, 255, 0.06);
  }

  .topbar-brand {
    font-size: 0.8125rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--text);
  }

  /* ── Sidebar backdrop (mobile) ──────────────────────────────── */
  .sidebar-backdrop {
    display: none;
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    backdrop-filter: blur(2px);
    z-index: 49;
  }

  /* ── Shell ───────────────────────────────────────────────────── */
  .shell {
    display: flex;
    height: 100vh;
  }

  /* ── Sidebar ─────────────────────────────────────────────────── */
  .sidebar {
    width: 11.5rem;
    flex-shrink: 0;
    background: var(--bg-panel);
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border);
    z-index: 50;
  }

  .logo {
    display: flex;
    align-items: center;
    gap: 0.625rem;
    padding: 0.875rem 1.25rem;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
    position: relative;
    z-index: 1;
  }

  .brand {
    font-size: 0.8125rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--text);
    text-transform: lowercase;
  }

  .nav-items {
    display: flex;
    flex-direction: column;
    padding: 0.375rem 0;
    flex: 1;
    overflow-y: auto;
  }

  .nav-btn {
    padding: 0.5rem 0.875rem;
    text-align: left;
    font-size: 0.6875rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    color: var(--text-dim);
    background: none;
    border: none;
    border-left: 2px solid transparent;
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .nav-icon {
    font-size: 0.6875rem;
    width: 1rem;
    text-align: center;
    opacity: 0.5;
  }

  .nav-btn:hover {
    color: var(--text);
    background: rgba(255, 255, 255, 0.035);
    border-left-color: rgba(255, 255, 255, 0.12);
  }

  .nav-btn.active {
    color: var(--accent);
    font-weight: 600;
    background: rgba(78, 168, 222, 0.07);
    border-left-color: var(--accent);
  }
  .nav-btn.active .nav-icon {
    opacity: 1;
  }

  /* ── Signed-in user (sidebar foot) ──────────────────────────── */
  .sidebar-user {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    padding: 0.75rem 0.875rem;
    border-top: 1px solid var(--border);
    font-size: 0.6875rem;
  }

  .user-name {
    color: var(--text);
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .logout-btn {
    align-self: flex-start;
    color: var(--text-dim);
    font-size: 0.6875rem;
    letter-spacing: 0.04em;
    padding: 0;
  }
  .logout-btn:hover:not(:disabled) {
    color: var(--accent);
  }
  .logout-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .logout-error {
    color: var(--bad);
  }

  /* ── Main content ────────────────────────────────────────────── */
  .content {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }

  .page-scroll {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: 1.5rem 2rem;
    scroll-behavior: smooth;
  }

  /* Opacity only, unlike the shared fadeIn: this element is the scroll
     container, and a transform changing on it switches scroll anchoring
     off for every frame of the animation, so cards mounting in those
     150ms push the content around instead of being absorbed. */
  .page-enter {
    animation: page-in 150ms ease both;
  }

  @keyframes page-in {
    from { opacity: 0; }
    to   { opacity: 1; }
  }

  .api-banner {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 1.5rem 2rem 0;
    padding: 0.5rem 0.75rem;
    background: var(--bg-elevated);
    border: 1px solid var(--bad);
    border-radius: var(--radius-sm);
    color: var(--text-dim);
    font-size: 0.75rem;
  }

  .api-banner-icon {
    color: var(--bad);
  }

  /* ── Mobile (< 768px) ────────────────────────────────────────── */
  @media (max-width: 767px) {
    .topbar {
      display: flex;
    }

    .sidebar-backdrop {
      display: block;
    }

    .shell {
      padding-top: 48px;
    }

    .sidebar {
      position: fixed;
      top: 0;
      left: 0;
      bottom: 0;
      transform: translateX(-100%);
      /* visibility (not just transform) is what drops the closed drawer out of
         the tab order and the accessibility tree; transitioning it keeps the
         panel rendered for the full slide-out before it flips to hidden. */
      visibility: hidden;
      transition:
        transform 200ms cubic-bezier(0.16, 1, 0.3, 1),
        visibility 200ms;
      box-shadow: 4px 0 20px rgba(0, 0, 0, 0.4);
    }

    .sidebar.open {
      transform: translateX(0);
      visibility: visible;
    }

    .page-scroll {
      padding: 1rem;
    }

    .api-banner {
      margin: 1rem 1rem 0;
    }
  }
</style>
