import { describe, expect, it } from "vitest";

import type { PluginCommand, PluginUiEntry } from "../api";
import {
  buildPluginCommandActions,
  isExternalHttpUrl,
  matchPluginChord,
  parsePluginChord,
  resolveCommandHref,
  resolveCommandLinks,
} from "../pluginCommands";

const badge: PluginCommand = {
  fqid: "plugin.acme.github.open_pr",
  plugin_id: "acme.github",
  id: "open_pr",
  title: "Open GitHub PR",
  description: "",
  keybinds: ["Ctrl+Shift+G"],
  action: { kind: "open-ui-link", slot: "row-badge", id: "github_pr_badge" },
};

function badgeEntry(items: unknown[], href?: string): PluginUiEntry {
  return {
    plugin_id: "acme.github",
    slot: "row-badge",
    id: "github_pr_badge",
    session_id: "s1",
    payload: href ? { items, href } : { items },
  };
}

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

describe("multi-repo workspaces", () => {
  const items = [
    { href: "https://github.com/o/a/pull/1", tooltip: "a: PR #1" },
    { href: "https://github.com/o/b/pull/2", tooltip: "b: PR #2" },
    { tooltip: "c: no PR" }, // no href -> skipped
  ];

  it("resolveCommandLinks returns one link per open PR from items", () => {
    const links = resolveCommandLinks(badge, [badgeEntry(items)], "s1");
    expect(links).toEqual([
      { href: "https://github.com/o/a/pull/1", label: "a: PR #1" },
      { href: "https://github.com/o/b/pull/2", label: "b: PR #2" },
    ]);
  });

  it("dedupes repeated hrefs", () => {
    const dup = [items[0], items[0]];
    expect(resolveCommandLinks(badge, [badgeEntry(dup)], "s1")).toHaveLength(1);
  });

  it("falls back to the top-level href when there are no item hrefs", () => {
    const links = resolveCommandLinks(badge, [badgeEntry([], "https://github.com/o/a/pull/9")], "s1");
    expect(links).toEqual([{ href: "https://github.com/o/a/pull/9", label: "https://github.com/o/a/pull/9" }]);
  });

  it("builds one palette entry per PR, titled by item label", () => {
    const actions = buildPluginCommandActions([badge], [badgeEntry(items)], "s1");
    expect(actions.map((a) => a.title)).toEqual(["Open GitHub PR: a: PR #1", "Open GitHub PR: b: PR #2"]);
    expect(actions.map((a) => a.id)).toEqual([
      "plugin:plugin.acme.github.open_pr:0",
      "plugin:plugin.acme.github.open_pr:1",
    ]);
    // No single-entry shortcut hint when the command fans out.
    expect(actions[0].shortcut).toBeUndefined();
  });

  it("builds a single titled entry when only one PR is open", () => {
    const actions = buildPluginCommandActions([badge], [badgeEntry([items[0]])], "s1");
    expect(actions).toHaveLength(1);
    expect(actions[0]).toMatchObject({ id: "plugin:plugin.acme.github.open_pr", title: "Open GitHub PR" });
    expect(actions[0].shortcut).toBe("Ctrl+Shift+G");
  });

  it("resolveCommandHref returns the top-level primary for the keybind", () => {
    expect(resolveCommandHref(badge, [badgeEntry(items, "https://github.com/o/b/pull/2")], "s1")).toBe(
      "https://github.com/o/b/pull/2",
    );
  });
});
