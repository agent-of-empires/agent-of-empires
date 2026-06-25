import { describe, expect, it } from "vitest";
import type { PluginUiEntry, PluginUiSlot } from "../api";
import { entryText, entryTone, globalEntries, payloadStr, sessionEntries, toneClasses } from "../pluginUi";

function entry(slot: PluginUiSlot, over: Partial<PluginUiEntry> = {}): PluginUiEntry {
  return {
    plugin_id: "acme.kit",
    slot,
    id: "x",
    payload: {},
    ...over,
  };
}

describe("pluginUi selectors", () => {
  it("globalEntries keeps only matching-slot entries without a session_id", () => {
    const entries = [
      entry("status-bar", { id: "a" }),
      entry("status-bar", { id: "b", session_id: "s1" }), // has session => not global
      entry("card", { id: "c" }),
    ];
    const got = globalEntries(entries, "status-bar");
    expect(got.map((e) => e.id)).toEqual(["a"]);
  });

  it("sessionEntries scopes to one session and is empty for a missing id", () => {
    const entries = [
      entry("row-badge", { id: "a", session_id: "s1" }),
      entry("row-badge", { id: "b", session_id: "s2" }),
      entry("row-badge", { id: "c" }), // global, excluded
    ];
    expect(sessionEntries(entries, "row-badge", "s1").map((e) => e.id)).toEqual(["a"]);
    expect(sessionEntries(entries, "row-badge", undefined)).toEqual([]);
    // Tearing guard: an id for a session that no longer exists matches nothing.
    expect(sessionEntries(entries, "row-badge", "gone")).toEqual([]);
  });

  it("payloadStr and entryText read string fields, falling back to empty", () => {
    const e = entry("card", { payload: { title: "Hi", text: "ok", n: 3 } });
    expect(payloadStr(e, "title")).toBe("Hi");
    expect(payloadStr(e, "n")).toBe(""); // non-string
    expect(payloadStr(e, "missing")).toBe("");
    expect(entryText(e)).toBe("ok");
  });

  it("entryTone validates against the closed set", () => {
    expect(entryTone(entry("status-bar", { payload: { tone: "success" } }))).toBe("success");
    expect(entryTone(entry("status-bar", { payload: { tone: "rainbow" } }))).toBeUndefined();
    expect(entryTone(entry("status-bar"))).toBeUndefined();
  });

  it("toneClasses maps every tone to theme tokens and falls back to neutral", () => {
    expect(toneClasses("success")).toContain("status-running");
    expect(toneClasses("danger")).toContain("status-error");
    expect(toneClasses(undefined)).toContain("status-idle");
  });
});
