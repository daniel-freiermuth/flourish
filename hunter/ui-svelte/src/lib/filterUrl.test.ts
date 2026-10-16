import { describe, expect, it } from "vitest";
import { Filter, restoreFilter } from "./filter.svelte";
import { readConfidence, readSelection, readSort, writeSelection } from "./filterUrl";
import { parseHash } from "./navigation.svelte";

function linkedFilter(hash: string, key: "repo" | "type"): Filter {
  return new Filter(readSelection(parseHash(hash).params, key));
}

describe("deep-linked filter semantics", () => {
  it("distinguishes all from none before findings arrive and after new options arrive", () => {
    const all = linkedFilter("#inbox", "type");
    const none = linkedFilter("#inbox?type=", "type");
    expect(all.accepts("bug")).toBe(true);
    expect(all.accepts("future_type")).toBe(true);
    expect(none.accepts("bug")).toBe(false);
    expect(none.accepts("future_type")).toBe(false);

    const params = new URLSearchParams("type=bug");
    writeSelection(params, "type", none.selection);
    expect(new Filter(readSelection(params, "type")).accepts("bug")).toBe(false);
    writeSelection(params, "type", all.selection);
    expect(new Filter(readSelection(params, "type")).accepts("future_type")).toBe(true);
  });

  it("preserves absent selections, multiple values, and reserved characters", () => {
    const selected = new Filter(["team/a & b+?,#", "absent repo"]);
    const params = new URLSearchParams("sort=updated");
    writeSelection(params, "repo", selected.selection);
    const restored = linkedFilter(`#findings?${params.toString()}`, "repo");
    expect(restored.accepts("team/a & b+?,#")).toBe(true);
    expect(restored.accepts("absent repo")).toBe(true);
    expect(restored.accepts("unselected repo")).toBe(false);
    expect(params.get("sort")).toBe("updated");
  });

  it("does not turn an explicit list into all when the offered options happen to match", () => {
    const filter = linkedFilter("#findings?type=bug&type=test_gap", "type");
    expect(filter.allSelected(["bug", "test_gap"])).toBe(true);
    expect(filter.accepts("standards")).toBe(false);
    const params = new URLSearchParams();
    writeSelection(params, "type", filter.selection);
    expect(new Filter(readSelection(params, "type")).accepts("standards")).toBe(false);
  });

  it("keeps absent deep-linked values when the last offered option is selected", () => {
    const params = new URLSearchParams("repo=visible&repo=absent");
    const filter = new Filter(readSelection(params, "repo"));
    filter.toggle("other", ["visible", "other"]);
    writeSelection(params, "repo", filter.selection);
    const restored = new Filter(readSelection(params, "repo"));
    expect(restored.accepts("absent")).toBe(true);
    expect(restored.accepts("other")).toBe(true);
    expect(restored.accepts("arriving later")).toBe(false);
    expect(params.getAll("repo")).toContain("absent");
  });

  it("keeps offered values explicit when a toggle completes the current selection", () => {
    const params = new URLSearchParams("type=bug");
    const filter = new Filter(readSelection(params, "type"));
    filter.toggle("test_gap", ["bug", "test_gap"]);
    writeSelection(params, "type", filter.selection);
    const restored = new Filter(readSelection(params, "type"));
    expect(restored.accepts("bug")).toBe(true);
    expect(restored.accepts("test_gap")).toBe(true);
    expect(restored.accepts("standards")).toBe(false);
    expect(params.has("type")).toBe(true);
  });

  it("puts an option back after the URL round trip of unchecking it", () => {
    // FilterBar's flow: every click writes the URL, and the shown Filter
    // is restored from it before the next click.
    const options = ["bug", "perf", "style"];
    const params = new URLSearchParams();
    let shown = restoreFilter(undefined, readSelection(params, "type"));
    shown.toggle("perf", options);
    writeSelection(params, "type", shown.selection);
    shown = restoreFilter(shown, readSelection(params, "type"));
    shown.toggle("perf", options);
    writeSelection(params, "type", shown.selection);
    shown = restoreFilter(shown, readSelection(params, "type"));
    expect(params.has("type")).toBe(false);
    expect(shown.filtering).toBe(false);
    expect(shown.accepts("brand_new_type")).toBe(true);
  });

  it("restores a list the shown filter did not write as an explicit link", () => {
    // Back/Forward or a pasted link: the narrowing made from All in this
    // session does not carry over to a different list.
    const options = ["bug", "perf"];
    const shown = restoreFilter(undefined, null);
    shown.toggle("perf", options);
    const linked = restoreFilter(shown, ["perf"]);
    linked.toggle("bug", options);
    expect(linked.selection).toEqual(["bug", "perf"]);
    expect(linked.accepts("brand_new_type")).toBe(false);
  });

  it("parses filters independently of the existing focused-finding route", () => {
    const route = parseHash("#findings:123?type=bug&confidence=80&sort=newest");
    expect(route.page).toBe("findings");
    expect(route.focusId).toBe(123);
    expect(new Filter(readSelection(route.params, "type")).accepts("test_gap")).toBe(false);
    expect(readConfidence(route.params)).toBe(80);
    expect(readSort(route.params)).toBe("newest");
  });

  it("falls back safely for invalid confidence, sort, and finding ids", () => {
    for (const value of ["NaN", "-5", "105", "12", "Infinity"]) {
      expect(readConfidence(new URLSearchParams({ confidence: value }))).toBe(0);
    }
    expect(readConfidence(new URLSearchParams("confidence=100"))).toBe(100);
    expect(readSort(new URLSearchParams("sort=invalid"))).toBe("severity");
    for (const id of ["0", "-1", "12oops", "9007199254740992"]) {
      expect(parseHash(`#findings:${id}?type=bug`).focusId).toBeNull();
    }
  });
});
