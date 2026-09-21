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

vi.mock("../api", async (orig) => ({
  ...(await orig<typeof import("../api")>()),
  invokePluginCommand: vi.fn().mockResolvedValue(true),
}));
import { invokePluginCommand } from "../api";

vi.mock("../toastBus", () => ({ reportError: vi.fn() }));
import { reportError } from "../toastBus";

const openPr: PluginCommand = {
  fqid: "plugin.acme.github.open_pr",
  plugin_id: "acme.github",
  id: "open_pr",
  title: "Open GitHub PR",
  description: "",
  keybinds: ["Ctrl+Shift+G"],
  action: { kind: "open-ui-link", slot: "row-column", id: "pr" },
};
const refresh: PluginCommand = {
  ...openPr,
  fqid: "plugin.acme.github.refresh",
  id: "refresh",
  title: "Refresh PRs",
  keybinds: ["Ctrl+Shift+R"],
  action: null,
};

const entry = (payload: Record<string, unknown>, plugin_id = "acme.github"): PluginUiEntry => ({
  plugin_id,
  slot: "row-column",
  id: "pr",
  session_id: "s1",
  payload,
});
const prA = { href: "https://github.com/o/a/pull/1", tooltip: "a: PR #1" };
const prB = { href: "https://github.com/o/b/pull/2", tooltip: "b: PR #2" };
const key = (k: string, over: Partial<KeyboardEvent> = {}) =>
  ({ ctrlKey: true, shiftKey: true, altKey: false, metaKey: false, key: k, ...over }) as KeyboardEvent;

it.each([
  ["https://x.test", true],
  ["http://x.test", true],
  ["javascript:alert(1)", false],
  ["file:///etc/passwd", false],
  ["", false],
  [undefined, false],
])("isExternalHttpUrl(%j) is %s", (url, expected) => {
  expect(isExternalHttpUrl(url)).toBe(expected);
});

describe("chords", () => {
  it.each([
    ["Ctrl+Shift+G", { ctrl: true, shift: true, alt: false, meta: false, base: "g" }],
    ["g+h", null],
    ["Ctrl+Shift", null],
  ])("parsePluginChord(%j)", (chord, expected) => {
    expect(parsePluginChord(chord)).toEqual(expected);
  });

  it("matchPluginChord needs every modifier and the base key", () => {
    const chord = parsePluginChord("Ctrl+Shift+G")!;
    expect(matchPluginChord(chord, key("G"))).toBe(true);
    expect(matchPluginChord(chord, key("g", { shiftKey: false }))).toBe(false);
  });
});

describe("resolveCommandLinks", () => {
  it.each<[string, Record<string, unknown>, { href: string; label: string }[]]>([
    [
      "one link per item href",
      { items: [prA, prB, { tooltip: "c: no PR" }] },
      [
        { href: prA.href, label: prA.tooltip },
        { href: prB.href, label: prB.tooltip },
      ],
    ],
    ["deduped hrefs", { items: [prA, prA] }, [{ href: prA.href, label: prA.tooltip }]],
    ["malformed items skipped", { items: [null, "nope", 42, prA] }, [{ href: prA.href, label: prA.tooltip }]],
    [
      "the top-level href fallback",
      { items: [], href: "https://github.com/o/a/pull/9" },
      [{ href: "https://github.com/o/a/pull/9", label: "https://github.com/o/a/pull/9" }],
    ],
  ])("%s", (_name, payload, expected) => {
    expect(resolveCommandLinks(openPr, [entry(payload)], "s1")).toEqual(expected);
  });
});

describe("buildPluginCommandActions", () => {
  it("builds one titled entry for a single link, and none without a link", () => {
    const actions = buildPluginCommandActions([openPr], [entry({ href: prA.href })], "s1");
    expect(actions).toEqual([
      expect.objectContaining({
        id: "plugin:plugin.acme.github.open_pr",
        title: "Open GitHub PR",
        group: "Actions",
        shortcut: "Ctrl+Shift+G",
      }),
    ]);
    expect(buildPluginCommandActions([openPr], [], "s1")).toEqual([]);
  });

  it("builds one entry per PR in a multi-repo workspace", () => {
    const actions = buildPluginCommandActions([openPr], [entry({ items: [prA, prB] })], "s1");
    expect(actions.map((a) => [a.id, a.title, a.shortcut])).toEqual([
      ["plugin:plugin.acme.github.open_pr:0", "Open GitHub PR: a: PR #1", undefined],
      ["plugin:plugin.acme.github.open_pr:1", "Open GitHub PR: b: PR #2", undefined],
    ]);
  });

  it("invokes an action-less command only with an active session", () => {
    expect(buildPluginCommandActions([refresh], [], null)).toEqual([]);
    const [action] = buildPluginCommandActions([refresh], [], "s1");
    expect(action).toMatchObject({
      id: "plugin:plugin.acme.github.refresh",
      title: "Refresh PRs",
      shortcut: "Ctrl+Shift+R",
    });
    vi.mocked(invokePluginCommand).mockClear();
    action!.perform();
    expect(invokePluginCommand).toHaveBeenCalledWith("plugin.acme.github.refresh", "s1");
  });
});

describe("invokeActionlessCommand", () => {
  it("error-toasts only when the invocation is rejected", async () => {
    vi.mocked(reportError).mockClear();
    vi.mocked(invokePluginCommand).mockResolvedValueOnce(true);
    invokeActionlessCommand(refresh, "s1");
    await new Promise((r) => setTimeout(r, 0));
    expect(reportError).not.toHaveBeenCalled();
    vi.mocked(invokePluginCommand).mockResolvedValueOnce(false);
    invokeActionlessCommand(refresh, "s1");
    await vi.waitFor(() => expect(reportError).toHaveBeenCalledWith("Failed to run Refresh PRs"));
  });
});

describe("pickKeybindEffect", () => {
  const cmdA: PluginCommand = { ...openPr, fqid: "plugin.acme.a.open", plugin_id: "acme.a" };
  const cmdB: PluginCommand = { ...cmdA, fqid: "plugin.acme.b.open", plugin_id: "acme.b" };

  it.each<[string, PluginCommand[], PluginUiEntry[], string | null, KeyboardEvent, unknown]>([
    [
      "opens a single link",
      [cmdA],
      [entry({ href: "https://x.test/1" }, "acme.a")],
      "s1",
      key("g"),
      { kind: "open", href: "https://x.test/1" },
    ],
    [
      "picks among several links",
      [cmdA],
      [
        entry(
          {
            items: [
              { href: "https://x.test/1", tooltip: "one" },
              { href: "https://x.test/2", tooltip: "two" },
            ],
          },
          "acme.a",
        ),
      ],
      "s1",
      key("g"),
      {
        kind: "pick",
        links: [
          { href: "https://x.test/1", label: "one" },
          { href: "https://x.test/2", label: "two" },
        ],
      },
    ],
    ["invokes an action-less command", [refresh], [], "s1", key("r"), { kind: "invoke", cmd: refresh }],
    ["skips an action-less command without a session", [refresh], [], null, key("r"), null],
    [
      "falls through to a later command sharing the chord",
      [cmdA, cmdB],
      [entry({ href: "https://x.test/2" }, "acme.b")],
      "s1",
      key("g"),
      { kind: "open", href: "https://x.test/2" },
    ],
    ["returns null when nothing can execute", [cmdA, cmdB], [], "s1", key("g"), null],
    [
      "ignores non-matching chords",
      [cmdA],
      [entry({ href: "https://x.test/1" }, "acme.a")],
      "s1",
      key("x", { ctrlKey: false, shiftKey: false }),
      null,
    ],
  ])("%s", (_name, commands, entries, session, event, expected) => {
    expect(pickKeybindEffect(commands, entries, session, event)).toEqual(expected);
  });
});
