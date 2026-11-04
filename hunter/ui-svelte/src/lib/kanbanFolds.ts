// Which Kanban columns the operator folded, remembered per browser across
// reloads. Keys are the column keys of KanbanPage; a key no column has
// any more is carried along harmlessly.

const STORAGE_KEY = "hunter.kanban.folded";

// Storage can be unavailable (disabled, sandboxed iframe, a test runtime
// without it) and then throws on access. Like the Stats horizon, this is
// a convenience: without storage every column just opens unfolded.

/** The remembered folded column keys; none when nothing (valid) is stored. */
export function loadFoldedColumns(): string[] {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "[]");
    return Array.isArray(parsed) ? parsed.filter((k): k is string => typeof k === "string") : [];
  } catch {
    return [];
  }
}

export function saveFoldedColumns(keys: Iterable<string>): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify([...keys]));
  } catch {
    // See above: not remembered, nothing else lost.
  }
}
