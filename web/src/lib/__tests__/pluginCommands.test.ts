import { describe, expect, it } from "vitest";

import type { PluginCommand, PluginUiEntry } from "../api";
import {
  buildPluginCommandActions,
  isExternalHttpUrl,
  matchPluginChord,
  parsePluginChord,
  resolveCommandHref,
} from "../pluginCommands";

const openPr: PluginCommand = {
  fqid: "plugin.acme.github.open_pr",
  plugin_id: "acme.github",
  id: "open_pr",
  title: "Open GitHub PR",
  description: "Open the active session's PR",
  keybinds: ["Ctrl+Shift+G"],
  action: { kind: "open-ui-link", slot: "row-column", id: "pr" },
};

function entry(over: Partial<PluginUiEntry>): PluginUiEntry {
  return {
    plugin_id: "acme.github",
    slot: "row-column",
    id: "pr",
    session_id: "s1",
    payload: { href: "https://github.com/o/r/pull/12" },
    ...over,
  };
}

describe("isExternalHttpUrl", () => {
  it("accepts http/https and rejects everything else", () => {
    expect(isExternalHttpUrl("https://x.test")).toBe(true);
    expect(isExternalHttpUrl("http://x.test")).toBe(true);
    expect(isExternalHttpUrl("javascript:alert(1)")).toBe(false);
    expect(isExternalHttpUrl("file:///etc/passwd")).toBe(false);
    expect(isExternalHttpUrl("")).toBe(false);
    expect(isExternalHttpUrl(undefined)).toBe(false);
  });
});

describe("resolveCommandHref", () => {
  it("returns the href for the matching active-session entry", () => {
    expect(resolveCommandHref(openPr, [entry({})], "s1")).toBe("https://github.com/o/r/pull/12");
  });
  it("returns null with no active session", () => {
    expect(resolveCommandHref(openPr, [entry({})], null)).toBeNull();
  });
  it("ignores entries for another session", () => {
    expect(resolveCommandHref(openPr, [entry({ session_id: "other" })], "s1")).toBeNull();
  });
  it("ignores another plugin's entry at the same slot/id", () => {
    expect(resolveCommandHref(openPr, [entry({ plugin_id: "evil" })], "s1")).toBeNull();
  });
  it("rejects an unsafe href", () => {
    expect(resolveCommandHref(openPr, [entry({ payload: { href: "javascript:1" } })], "s1")).toBeNull();
  });
});

describe("buildPluginCommandActions", () => {
  it("includes an open-ui-link command when its href resolves", () => {
    const actions = buildPluginCommandActions([openPr], [entry({})], "s1");
    expect(actions).toHaveLength(1);
    expect(actions[0]).toMatchObject({ id: "plugin:plugin.acme.github.open_pr", group: "Actions" });
  });
  it("hides the command when no href resolves", () => {
    expect(buildPluginCommandActions([openPr], [], "s1")).toHaveLength(0);
  });
  it("skips commands without a client action", () => {
    const noAction: PluginCommand = { ...openPr, action: null };
    expect(buildPluginCommandActions([noAction], [entry({})], "s1")).toHaveLength(0);
  });
});

describe("parsePluginChord", () => {
  it("parses modifiers plus a base key", () => {
    expect(parsePluginChord("Ctrl+Shift+G")).toEqual({
      ctrl: true,
      shift: true,
      alt: false,
      meta: false,
      base: "g",
    });
  });
  it("returns null for two base keys", () => {
    expect(parsePluginChord("g+h")).toBeNull();
  });
  it("returns null with no base key", () => {
    expect(parsePluginChord("Ctrl+Shift")).toBeNull();
  });
});

describe("matchPluginChord", () => {
  const chord = parsePluginChord("Ctrl+Shift+G")!;
  it("matches an exact event", () => {
    const e = { ctrlKey: true, shiftKey: true, altKey: false, metaKey: false, key: "G" } as KeyboardEvent;
    expect(matchPluginChord(chord, e)).toBe(true);
  });
  it("does not match when a modifier differs", () => {
    const e = { ctrlKey: true, shiftKey: false, altKey: false, metaKey: false, key: "g" } as KeyboardEvent;
    expect(matchPluginChord(chord, e)).toBe(false);
  });
});
