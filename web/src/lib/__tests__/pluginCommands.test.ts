import { describe, expect, it, vi } from "vitest";

import type { PluginCommand, PluginUiEntry } from "../api";
import {
  buildPluginCommandActions,
  invokeActionlessCommand,
  isExternalHttpUrl,
  matchPluginChord,
  parsePluginChord,
  pickKeybindEffect,
  resolveCommandLinks,
} from "../pluginCommands";

// buildPluginCommandActions' action-less entries dispatch through invokePluginCommand;
// mock just that export so performing one records the call instead of hitting the network.
vi.mock("../api", async (orig) => ({
  ...(await orig<typeof import("../api")>()),
  invokePluginCommand: vi.fn().mockResolvedValue(true),
}));
import { invokePluginCommand } from "../api";

vi.mock("../toastBus", () => ({ reportError: vi.fn() }));
import { reportError } from "../toastBus";

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

// An action-less command (no client action): invoked through the worker path.
const refresh: PluginCommand = {
  fqid: "plugin.acme.github.refresh",
  plugin_id: "acme.github",
  id: "refresh",
  title: "Refresh PRs",
  description: "Re-fetch open PRs",
  keybinds: ["Ctrl+Shift+R"],
  action: null,
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

describe("buildPluginCommandActions", () => {
  it("includes an open-ui-link command when its href resolves", () => {
    const actions = buildPluginCommandActions([openPr], [entry({})], "s1");
    expect(actions).toHaveLength(1);
    expect(actions[0]).toMatchObject({ id: "plugin:plugin.acme.github.open_pr", group: "Actions" });
  });
  it("hides the open-ui-link command when no href resolves", () => {
    expect(buildPluginCommandActions([openPr], [], "s1")).toHaveLength(0);
  });
  it("includes an action-less command as an invoke entry when a session is active", () => {
    const actions = buildPluginCommandActions([refresh], [], "s1");
    expect(actions).toHaveLength(1);
    expect(actions[0]).toMatchObject({
      id: "plugin:plugin.acme.github.refresh",
      title: "Refresh PRs",
      shortcut: "Ctrl+Shift+R",
    });
    vi.mocked(invokePluginCommand).mockClear();
    vi.mocked(invokePluginCommand).mockResolvedValue(true);
    actions[0].perform();
    expect(invokePluginCommand).toHaveBeenCalledWith("plugin.acme.github.refresh", "s1");
  });
  it("omits an action-less command when there is no active session", () => {
    expect(buildPluginCommandActions([refresh], [], null)).toHaveLength(0);
  });
});

describe("invokeActionlessCommand", () => {
  it("error-toasts when the invocation is rejected", async () => {
    vi.mocked(invokePluginCommand).mockResolvedValueOnce(false);
    vi.mocked(reportError).mockClear();
    invokeActionlessCommand(refresh, "s1");
    await vi.waitFor(() => expect(reportError).toHaveBeenCalledWith("Failed to run Refresh PRs"));
  });

  it("does not toast when the invocation succeeds", async () => {
    vi.mocked(invokePluginCommand).mockResolvedValueOnce(true);
    vi.mocked(reportError).mockClear();
    invokeActionlessCommand(refresh, "s1");
    await new Promise((r) => setTimeout(r, 0));
    expect(reportError).not.toHaveBeenCalled();
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

  it("skips malformed (null/primitive) item entries without throwing", () => {
    const mixed = [null, "nope", 42, items[0]];
    const links = resolveCommandLinks(badge, [badgeEntry(mixed)], "s1");
    expect(links).toEqual([{ href: "https://github.com/o/a/pull/1", label: "a: PR #1" }]);
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
});

describe("pickKeybindEffect", () => {
  const ev = { ctrlKey: true, shiftKey: true, altKey: false, metaKey: false, key: "g" } as KeyboardEvent;
  const cmdA: PluginCommand = {
    fqid: "plugin.acme.a.open",
    plugin_id: "acme.a",
    id: "open",
    title: "A",
    description: "",
    keybinds: ["Ctrl+Shift+G"],
    action: { kind: "open-ui-link", slot: "row-column", id: "pr" },
  };
  const cmdB: PluginCommand = { ...cmdA, fqid: "plugin.acme.b.open", plugin_id: "acme.b" };

  function entryFor(pluginId: string, href: string): PluginUiEntry {
    return { plugin_id: pluginId, slot: "row-column", id: "pr", session_id: "s1", payload: { href } };
  }

  it("opens the matching command's single link", () => {
    expect(pickKeybindEffect([cmdA], [entryFor("acme.a", "https://x.test/1")], "s1", ev)).toEqual({
      kind: "open",
      href: "https://x.test/1",
    });
  });

  it("returns a picker for a chord that resolves to several links", () => {
    const multi: PluginUiEntry = {
      plugin_id: "acme.a",
      slot: "row-column",
      id: "pr",
      session_id: "s1",
      payload: {
        items: [
          { href: "https://x.test/1", tooltip: "one" },
          { href: "https://x.test/2", tooltip: "two" },
        ],
      },
    };
    expect(pickKeybindEffect([cmdA], [multi], "s1", ev)).toEqual({
      kind: "pick",
      links: [
        { href: "https://x.test/1", label: "one" },
        { href: "https://x.test/2", label: "two" },
      ],
    });
  });

  it("invokes an action-less command with an active session", () => {
    const refreshEv = { ctrlKey: true, shiftKey: true, altKey: false, metaKey: false, key: "r" } as KeyboardEvent;
    expect(pickKeybindEffect([refresh], [], "s1", refreshEv)).toEqual({
      kind: "invoke",
      cmd: refresh,
    });
    // No session: nothing to scope the invocation to.
    expect(pickKeybindEffect([refresh], [], null, refreshEv)).toBeNull();
  });

  it("falls through to a later command sharing the chord when the first is inactive", () => {
    // cmdA matches the chord but has no entry (inactive for this session); cmdB
    // shares the chord and resolves, so it must still fire.
    expect(pickKeybindEffect([cmdA, cmdB], [entryFor("acme.b", "https://x.test/2")], "s1", ev)).toEqual({
      kind: "open",
      href: "https://x.test/2",
    });
  });

  it("returns null when no matching command can execute", () => {
    expect(pickKeybindEffect([cmdA, cmdB], [], "s1", ev)).toBeNull();
  });

  it("ignores commands whose chord does not match", () => {
    const other = { ctrlKey: false, shiftKey: false, altKey: false, metaKey: false, key: "x" } as KeyboardEvent;
    expect(pickKeybindEffect([cmdA], [entryFor("acme.a", "https://x.test/1")], "s1", other)).toBeNull();
  });
});
