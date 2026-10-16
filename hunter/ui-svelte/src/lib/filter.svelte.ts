import { SvelteSet } from "svelte/reactivity";

/**
 * One filter dimension of the filter bar (repo, type, class, severity,
 * status), as a two-state value.
 *
 * The whole semantics is this: a dimension is either "all", meaning
 * everything including options nobody has seen yet, or "only", meaning
 * exactly the listed values and nothing else. That second state is what
 * makes the model work — it is a statement about the future as well as
 * the present, so an option that turns up later is covered by it.
 *
 * The previous model could not express that. It kept the selected values
 * plus a parallel set of every option ever seen, and auto-selected each
 * option the first time it appeared, so that new data could not silently
 * hide itself. But "the user switched this dimension off" was not
 * representable, so a newly arriving option had nothing to inherit and
 * took the default: after deselecting All, the next poll that brought a
 * new option selected it, the All box went indeterminate on its own, and
 * a finding the user had filtered out appeared in the list.
 *
 * Here "deselect All" is `only` with no values, and it stays that way
 * because a new option is not in the set. Nothing decays.
 *
 * Two properties worth stating, because they are the reason for the
 * shape rather than accidents of it:
 *
 * - In `all`, new options are visible immediately. This is the guarantee
 *   the old auto-select existed to provide, and it survives: no data can
 *   appear without being shown unless the user has said otherwise.
 * - In `only`, the values are remembered even when they are absent from
 *   the current options. A repo whose findings are all triaged still
 *   counts as chosen when it comes back, so the dimension does not
 *   quietly change meaning while the data moves underneath it.
 *
 * Every predicate takes the currently offered `options` rather than
 * caching them: which options exist is the caller's business and changes
 * on every poll, while what the user asked for is this object's business
 * and changes only when they act.
 */
export class Filter {
  #mode: "all" | "only" = $state("all");
  // A SvelteSet, and mutated in place: the instance is itself the
  // reactive value, so replacing it would cost every holder its
  // subscription.
  readonly #values = new SvelteSet<string>();
  // A restored list describes future options too; checking the last visible
  // option must not turn that explicit promise into an unrestricted filter.
  #explicitSelection = false;

  /** Restore an explicit list (including none), or accept all present and future values for null. */
  constructor(values: readonly string[] | null = null) {
    if (values !== null) {
      this.#mode = "only";
      this.#explicitSelection = true;
      for (const value of values) this.#values.add(value);
    }
  }

  /** null means all; an empty list means explicitly none, even before data loads. */
  get selection(): string[] | null {
    return this.#mode === "all" ? null : [...this.#values].sort();
  }

  /** Whether `value` passes this filter. */
  accepts(value: string): boolean {
    return this.#mode === "all" || this.#values.has(value);
  }

  /** Whether the user has narrowed this dimension at all. */
  get filtering(): boolean {
    return this.#mode === "only";
  }

  /**
   * Every currently-offered option passes.
   *
   * True in `all` regardless of what is on offer, including nothing:
   * "all of nothing" is still unfiltered, and a dimension with no
   * options must not render as if the user had excluded something.
   */
  allSelected(options: readonly string[]): boolean {
    return this.#mode === "all" || options.every((o) => this.#values.has(o));
  }

  /** No currently-offered option passes. Remembered absent values do not count. */
  noneSelected(options: readonly string[]): boolean {
    return this.#mode === "only" && !options.some((o) => this.#values.has(o));
  }

  /**
   * Flip one option.
   *
   * Unchecking from `all` has to name the survivors, since `all` has no
   * list to remove from — that is the point at which an implicit
   * "everything" becomes an explicit choice.
   */
  toggle(option: string, options: readonly string[]): void {
    if (this.#mode === "all") {
      this.#mode = "only";
      this.#values.clear();
      for (const o of options) {
        if (o !== option) this.#values.add(o);
      }
      return;
    }
    if (this.#values.has(option)) {
      this.#values.delete(option);
      return;
    }
    this.#values.add(option);
    // Only undo a narrowing originally made from implicit All. A restored
    // explicit list stays explicit, including values absent from options.
    if (!this.#explicitSelection && options.every((o) => this.#values.has(o))) this.reset();
  }

  /** Flip the All checkbox: everything, or nothing. */
  toggleAll(options: readonly string[]): void {
    if (this.allSelected(options)) {
      // Nothing, and it stays nothing: an option arriving later is not
      // in `#values`, so it is not quietly let back in.
      this.#mode = "only";
      this.#values.clear();
    } else {
      this.reset();
    }
  }

  /** Back to unfiltered, forgetting every remembered value. */
  reset(): void {
    this.#mode = "all";
    this.#explicitSelection = false;
    this.#values.clear();
  }
}

/**
 * Whether the selector must be rendered.
 *
 * A single-option dimension normally needs no control. The exception is
 * when that option is excluded: the filter then matches nothing, and
 * hiding the control would leave no way back short of a reload.
 */
export function needsSelector(options: readonly string[], filter: Filter): boolean {
  return options.length > 1 || !filter.allSelected(options);
}

/**
 * The Filter a selector shows for the selection its URL now holds.
 *
 * `previous` is the instance shown before this URL change.
 */
export function restoreFilter(previous: Filter | undefined, values: readonly string[] | null): Filter {
  void previous;
  return new Filter(values);
}
