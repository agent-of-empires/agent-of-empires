// @vitest-environment jsdom

import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginUiEntry } from "../../../lib/api";
import { PluginPaneBody } from "../PluginPane";
import {
  PluginCards,
  PluginComposerActions,
  PluginHomePanes,
  PluginRowBadges,
  PluginStatusBarSegments,
} from "../PluginSlots";
import { composerDraftOperation } from "../composerDraftOperation";

const { entriesRef, refreshingRef, revisionRef, pokeMock, invokeMock } = vi.hoisted(() => ({
  entriesRef: { current: [] as PluginUiEntry[] },
  refreshingRef: { current: false },
  revisionRef: { current: 0 },
  pokeMock: vi.fn(),
  invokeMock: vi.fn(async (): Promise<{ baselineRevision: number | null } | null> => ({ baselineRevision: 0 })),
}));
vi.mock("../../../lib/pluginUiContext", () => ({
  usePluginUiEntries: () => entriesRef.current,
  usePluginUiRefreshing: () => refreshingRef.current,
  usePluginUiRevision: () => revisionRef.current,
  usePluginUiPoke: () => pokeMock,
}));
vi.mock("../../../lib/api", () => ({ invokePluginAction: invokeMock }));

const pane = (payload: Record<string, unknown>): PluginUiEntry => ({
  plugin_id: "acme.kit",
  slot: "pane",
  id: "p",
  session_id: "s1",
  payload,
});
const renderPane = (payload: Record<string, unknown>) => render(<PluginPaneBody entry={pane(payload)} />);
const renderBlocks = (...blocks: Record<string, unknown>[]) => renderPane({ blocks });
const rowBadge = (payload: Record<string, unknown>, session_id = "s1"): PluginUiEntry => ({
  plugin_id: "acme.kit",
  slot: "row-badge",
  id: "b",
  session_id,
  payload,
});
const REFRESH = { kind: "action", label: "Refresh", method: "github.refresh" };
const spinner = (c: HTMLElement) => c.querySelector("svg.animate-spin");

beforeEach(() => {
  entriesRef.current = [];
  refreshingRef.current = false;
  revisionRef.current = 0;
  pokeMock.mockClear();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => ({ baselineRevision: 0 }));
});

describe("plugin slots", () => {
  it("status-bar renders global segments and is empty otherwise", () => {
    const { container, rerender } = render(<PluginStatusBarSegments />);
    expect(container.textContent).toBe("");
    entriesRef.current = [
      { plugin_id: "acme.kit", slot: "status-bar", id: "s", payload: { text: "Build OK", tone: "success" } },
    ];
    rerender(<PluginStatusBarSegments />);
    expect(screen.getByText("Build OK")).toBeTruthy();
  });

  it("row-badge renders only the addressed session's entries as safe links with icons", async () => {
    entriesRef.current = [
      rowBadge({ text: "PR #12", icon: "git-pull-request-arrow", href: "https://github.com/o/r/pull/12" }),
      rowBadge({ text: "other" }, "s2"),
    ];
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    expect(screen.queryByText("other")).toBeNull();
    const link = screen.getByRole("link", { name: /PR #12/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/12");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toContain("noopener");
    await waitFor(() => expect(container.querySelector("svg")).toBeTruthy());
  });

  it("row-badge link click does not bubble to an ancestor's onClick", () => {
    entriesRef.current = [
      rowBadge({ text: "PR #12", icon: "git-pull-request-arrow", href: "https://github.com/o/r/pull/12" }),
    ];
    const rowClick = vi.fn();
    render(
      <div onClick={rowClick}>
        <PluginRowBadges sessionId="s1" />
      </div>,
    );
    fireEvent.click(screen.getByRole("link", { name: /PR #12/ }));
    expect(rowClick).not.toHaveBeenCalled();
  });

  it("row-badge with an unknown icon or unsafe href renders plain text", () => {
    entriesRef.current = [
      rowBadge({ items: [{ text: "evil", icon: "not-a-real-icon", href: "javascript:alert(1)" }] }),
    ];
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    expect(screen.getByText("evil")).toBeTruthy();
    expect(screen.queryByRole("link")).toBeNull();
    expect(container.querySelector("svg")).toBeNull();
  });

  it("row-badge items render one icon-only link each, named by tooltip and not truncated", async () => {
    entriesRef.current = [
      rowBadge({
        items: [
          { icon: "git-pull-request-arrow", tone: "success", href: "https://x/pr/1", tooltip: "PR #1" },
          { icon: "git-pull-request-draft", tone: "warn", href: "https://x/pr/2", tooltip: "PR #2" },
        ],
      }),
    ];
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    const links = screen.getAllByRole("link");
    expect(links.map((l) => l.getAttribute("href"))).toEqual(["https://x/pr/1", "https://x/pr/2"]);
    await waitFor(() => expect(container.querySelectorAll("svg")).toHaveLength(2));
    expect(screen.getByRole("link", { name: "PR #1" })).toBeTruthy();
    for (const link of links) {
      expect(link.className).not.toContain("truncate");
      expect(link.className).toContain("shrink-0");
    }
  });

  it("row-badge empty items clears the row", () => {
    entriesRef.current = [rowBadge({ items: [] })];
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    expect(container.querySelector("a, span")).toBeNull();
  });

  it("card renders title and body", () => {
    entriesRef.current = [
      { plugin_id: "acme.kit", slot: "card", id: "c", payload: { title: "Coverage", body: "92%" } },
    ];
    render(<PluginCards />);
    expect(screen.getByText("Coverage")).toBeTruthy();
    expect(screen.getByText("92%")).toBeTruthy();
  });

  it("home-pane renders blocks, and the simple form's title only once", () => {
    entriesRef.current = [
      {
        plugin_id: "acme.diag",
        slot: "home-pane",
        id: "m",
        payload: { title: "memory", blocks: [{ kind: "sparkline", values: [1, 2, 3] }] },
      },
      { plugin_id: "acme.disk", slot: "home-pane", id: "d", payload: { title: "Disk", body: "42% used" } },
    ];
    render(<PluginHomePanes />);
    expect(screen.getByText("memory")).toBeTruthy();
    expect(screen.getByTestId("plugin-pane-sparkline")).toBeTruthy();
    expect(screen.getAllByText("Disk")).toHaveLength(1);
    expect(screen.getByText("42% used")).toBeTruthy();
  });

  it("composer action forwards a composer snapshot to the worker", async () => {
    entriesRef.current = [
      {
        plugin_id: "acme.voice",
        slot: "composer-action",
        id: "dictate",
        session_id: "s1",
        payload: { label: "Voice", method: "voice.start", icon: "mic" },
      },
    ];
    const getSnapshot = () => ({ text: "hello", selectionStart: 1, selectionEnd: 5 });
    render(<PluginComposerActions sessionId="s1" getSnapshot={getSnapshot} />);
    fireEvent.click(screen.getByTestId("plugin-composer-action"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("acme.voice", "voice.start", "s1", {
        composer: { text: "hello", selection_start: 1, selection_end: 5 },
      }),
    );
    expect(pokeMock).toHaveBeenCalled();
  });

  it.each([
    [
      { kind: "insert-text", id: "op-1", text: "hello" },
      { id: "op-1", operation: { kind: "insert-text", text: "hello" } },
    ],
    [
      { kind: "set-text", id: "op-2", text: "" },
      { id: "op-2", operation: { kind: "set-text", text: "" } },
    ],
    [{ kind: "bad", id: "op-2", text: "hello" }, null],
  ])("composerDraftOperation parses %j", (draft_operation, expected) => {
    const entry: PluginUiEntry = { plugin_id: "a", slot: "composer-action", id: "d", payload: { draft_operation } };
    expect(composerDraftOperation(entry)).toEqual(expected);
  });
});

describe("pane actions", () => {
  it("forwards the named worker method with empty params", async () => {
    renderBlocks(REFRESH);
    const btn = screen.getByTestId("plugin-pane-action");
    expect(btn.textContent).toContain("Refresh");
    fireEvent.click(btn);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("acme.kit", "github.refresh", "s1", {}));
  });

  it("holds the spinner until the plugin revision advances, not just until the POST resolves", async () => {
    revisionRef.current = 7;
    invokeMock.mockImplementationOnce(async () => ({ baselineRevision: 7 }));
    const entry = pane({ blocks: [REFRESH] });
    const { container, rerender } = render(<PluginPaneBody entry={entry} />);
    const btn = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;
    fireEvent.click(btn);
    await waitFor(() => expect(pokeMock).toHaveBeenCalled());
    expect(spinner(container)).toBeTruthy();
    expect(btn.getAttribute("aria-busy")).toBe("true");
    expect(btn.disabled).toBe(true);

    revisionRef.current = 8;
    rerender(<PluginPaneBody entry={entry} />);
    await waitFor(() => expect(spinner(container)).toBeNull());
    expect(btn.getAttribute("aria-busy")).toBeNull();
    expect(btn.disabled).toBe(false);
  });

  it("clears a stuck spinner after the timeout when no fresh state arrives", async () => {
    vi.useFakeTimers();
    try {
      revisionRef.current = 3;
      invokeMock.mockImplementationOnce(async () => ({ baselineRevision: 3 }));
      const { container } = renderBlocks(REFRESH);
      const btn = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;
      await act(async () => {
        fireEvent.click(btn);
      });
      expect(spinner(container)).toBeTruthy();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15000);
      });
      expect(spinner(container)).toBeNull();
      expect(btn.disabled).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it.each([
    ["the daemon omits a baseline", { baselineRevision: null }],
    ["the POST fails", null],
  ])("clears once the POST settles when %s", async (_, result) => {
    invokeMock.mockImplementationOnce(async () => result);
    const { container } = renderBlocks(REFRESH);
    const btn = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;
    fireEvent.click(btn);
    await waitFor(() => expect(invokeMock).toHaveBeenCalled());
    await waitFor(() => {
      expect(spinner(container)).toBeNull();
      expect(btn.disabled).toBe(false);
    });
  });

  it("a callout stretches its actions; a disabled action never posts", () => {
    renderBlocks({
      kind: "callout",
      tone: "danger",
      icon: "circle-x",
      title: "2 required checks failing",
      detail: "Merging is blocked until Clippy passes.",
      actions: [{ kind: "action", label: "Merge blocked", method: "gh.merge", disabled: true }],
    });
    expect(screen.getByText("2 required checks failing")).toBeTruthy();
    expect(screen.getByText("Merging is blocked until Clippy passes.")).toBeTruthy();
    const button = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.click(button);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("an action with an href and no method links out instead of posting", () => {
    renderBlocks({
      kind: "action",
      label: "Squash and merge",
      href: "https://github.com/o/r/pull/1",
      variant: "primary",
    });
    const link = screen.getByRole("link", { name: /Squash and merge/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/1");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("a row with a method posts its params, marks selection, and keeps its href as a separate link", async () => {
    renderBlocks({
      kind: "row",
      prefix: "#3231",
      label: "fix(update): warn when daemon is stale",
      sublabel: "japanese · njbrake",
      method: "gh.select_pr",
      params: { repo: "o/r", number: 3231 },
      selected: true,
      href: "https://github.com/o/r/pull/3231",
      badges: [{ icon: "circle-x", tone: "danger", tooltip: "CI failing" }],
    });
    const button = screen.getByRole("button", { name: /fix\(update\)/ });
    expect(button.getAttribute("aria-pressed")).toBe("true");
    const link = screen.getByRole("link", { name: /Open .* externally/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/3231");
    expect(screen.getByLabelText("CI failing")).toBeTruthy();
    await act(async () => {
      fireEvent.click(button);
    });
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("acme.kit", "gh.select_pr", "s1", { repo: "o/r", number: 3231 }),
    );
  });
});

describe("pane blocks", () => {
  it.each([
    ["action without a method", { blocks: [{ kind: "action", label: "Refresh" }] }, "plugin-pane-action"],
    ["callout without title or detail", { blocks: [{ kind: "callout", tone: "danger" }] }, "plugin-pane-callout"],
    ["bar without a positive segment", { blocks: [{ kind: "bar", segments: [{ value: 0 }] }] }, "plugin-pane-bar"],
    ["columns without children", { blocks: [{ kind: "columns", children: [] }] }, "plugin-pane-columns"],
    ["empty footer", { blocks: [{ kind: "heading", text: "GitHub" }], footer: {} }, "plugin-pane-footer"],
  ])("%s renders nothing", (_, payload, testId) => {
    renderPane(payload);
    expect(screen.queryByTestId(testId)).toBeNull();
  });

  it("renders the simple title/body form and a refresh indicator only while polling", () => {
    const entry = pane({ title: "Logs", body: "tail..." });
    refreshingRef.current = true;
    const { rerender } = render(<PluginPaneBody entry={entry} />);
    expect(screen.getByText("Logs")).toBeTruthy();
    expect(screen.getByText("tail...")).toBeTruthy();
    expect(screen.getByTestId("plugin-pane-refreshing")).toBeTruthy();
    refreshingRef.current = false;
    rerender(<PluginPaneBody entry={{ ...entry, payload: { ...entry.payload } }} />);
    expect(screen.queryByTestId("plugin-pane-refreshing")).toBeNull();
  });

  it("renders heading, linked row, note, divider and skips unknown kinds", () => {
    const { container } = renderBlocks(
      { kind: "heading", text: "GitHub" },
      {
        kind: "row",
        icon: "git-pull-request-arrow",
        tone: "success",
        label: "nexus",
        value: "PR #12",
        sublabel: "o/nexus",
        href: "https://github.com/o/nexus/pull/12",
      },
      { kind: "note", text: "3 repos have no open PR", tone: "neutral" },
      { kind: "divider" },
      { kind: "some-future-kind", payload: { nested: true } },
    );
    expect(screen.getByText("GitHub")).toBeTruthy();
    expect(screen.getByText("3 repos have no open PR")).toBeTruthy();
    const link = screen.getByRole("link", { name: /nexus/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/nexus/pull/12");
    expect(container.querySelector("hr")).toBeTruthy();
  });

  it("a row tints via a validated hex color only; prefix-only and avatar rows render", () => {
    renderBlocks(
      { kind: "row", icon: "git-merge", label: "nexus", value: "MERGED #12", color: "#8957e5" },
      { kind: "row", label: "other", value: "open", color: "javascript:alert(1)" },
      { kind: "row", prefix: "◉", tone: "success" },
      { kind: "row", label: "Nate Brake", value: "approved", avatar: "NB" },
    );
    expect(screen.getByText("MERGED #12").style.color).toBe("rgb(137, 87, 229)");
    expect(screen.getByText("open").style.color).toBe("");
    expect(screen.getByText("◉")).toBeTruthy();
    expect(screen.getByText("NB")).toBeTruthy();
    expect(screen.getByText("approved").className).toContain("ml-auto");
  });

  it("sparkline colors each segment by the band its right-hand value reaches", () => {
    const bands = [
      { at: 70, tone: "warn" },
      { at: 90, tone: "danger" },
    ];
    const { container } = renderBlocks({ kind: "sparkline", values: [10, 70, 95], max: 100, bands });
    const lines = container.querySelectorAll("line");
    expect(lines).toHaveLength(2);
    expect(lines[0]!.getAttribute("class")).toContain("text-status-waiting");
    expect(lines[1]!.getAttribute("class")).toContain("text-status-error");
  });

  it("sparkline renders a band-colored point for one value", () => {
    const { container } = renderBlocks({
      kind: "sparkline",
      values: [95],
      max: 100,
      bands: [{ at: 90, tone: "danger" }],
    });
    expect(container.querySelector("circle")?.getAttribute("class")).toContain("text-status-error");
  });

  it("a bar sizes segments proportionally and drops non-positive values", () => {
    const { container } = renderBlocks({
      kind: "bar",
      caption: "18 files",
      segments: [
        { value: 750, tone: "success" },
        { value: 250, tone: "danger" },
        { value: 0, tone: "warn" },
        { tone: "info" },
      ],
    });
    const spans = Array.from(container.querySelectorAll<HTMLElement>("[data-testid='plugin-pane-bar'] > div > span"));
    expect(spans.map((s) => s.style.width)).toEqual(["75%", "25%"]);
    expect(screen.getByText("18 files")).toBeTruthy();
  });

  it("columns lay children side by side, and a lone child spans the full width", () => {
    const two = renderBlocks({
      kind: "columns",
      children: [
        { kind: "section", title: "DIFF", children: [{ kind: "row", value: "+842 -317" }] },
        { kind: "section", title: "LINKED", children: [{ kind: "row", prefix: "#3180", label: "Stale daemon" }] },
      ],
    });
    expect(two.getByTestId("plugin-pane-columns").className).toContain("grid-cols-2");
    expect(screen.getByText("Stale daemon")).toBeTruthy();
    two.unmount();
    const one = renderBlocks({ kind: "columns", children: [{ kind: "section", title: "DIFF" }] });
    expect(one.getByTestId("plugin-pane-columns").className).toContain("grid-cols-1");
  });

  it("collapsible sections render uncontrolled details; plain sections stay <section>", () => {
    const blocks = [
      { kind: "section", title: "Checks: passing", collapsible: true, children: [{ kind: "note", text: "ci" }] },
      {
        kind: "section",
        title: "Unresolved",
        collapsible: true,
        collapsed: true,
        children: [{ kind: "note", text: "cmt" }],
      },
      { kind: "section", title: "Plain", children: [{ kind: "note", text: "x" }] },
    ];
    const entry = pane({ blocks });
    const { container, rerender } = render(<PluginPaneBody entry={entry} />);
    const details = container.querySelectorAll("details");
    expect([...details].map((d) => d.open)).toEqual([true, false]);
    expect(screen.getByText("cmt")).toBeTruthy();
    expect(container.querySelector("section")).toBeTruthy();
    // A re-push must not undo the user's fold.
    details[0]!.open = false;
    rerender(<PluginPaneBody entry={{ ...entry, payload: { ...entry.payload } }} />);
    expect(container.querySelector("details")!.open).toBe(false);
  });

  it("a section title renders a tone-tinted icon", async () => {
    const { container } = renderBlocks({
      kind: "section",
      title: "Checks: passing",
      collapsible: true,
      collapsed: true,
      icon: "circle-check",
      tone: "success",
      children: [{ kind: "note", text: "ci" }],
    });
    const summary = container.querySelector("summary")!;
    expect(summary.className).toContain("text-status-running");
    await waitFor(() => expect(summary.querySelectorAll("svg")).toHaveLength(2));
  });

  it("a section header pins a value summary and badge pills", () => {
    renderBlocks({
      kind: "section",
      title: "CHECKS",
      value: "1 of 2 approved",
      value_tone: "warn",
      boxed: true,
      scroll: true,
      badges: [
        { text: "2 failing", tone: "danger" },
        { text: "17 passing", tone: "success" },
      ],
      children: [{ kind: "row", label: "Clippy" }],
    });
    for (const text of ["1 of 2 approved", "2 failing", "17 passing", "Clippy"]) {
      expect(screen.getByText(text)).toBeTruthy();
    }
  });

  it("comment blocks render read-only with author, location and resolved state", () => {
    renderBlocks({
      kind: "comment",
      author: "alice",
      body: "handle the nil case",
      path: "src/foo.py",
      line: 42,
      href: "https://github.com/o/r/pull/1#c1",
      resolved: false,
    });
    for (const text of ["alice", "handle the nil case", "src/foo.py:42", "unresolved"]) {
      expect(screen.getByText(text)).toBeTruthy();
    }
    expect(screen.getByRole("link").getAttribute("href")).toBe("https://github.com/o/r/pull/1#c1");
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("a long comment body is clamped with a more/less toggle", () => {
    const longBody = "x".repeat(250);
    renderBlocks({ kind: "comment", author: "bob", body: longBody });
    const body = screen.getByText(longBody);
    const toggle = screen.getByTestId("plugin-comment-toggle");
    expect(body.className).toContain("line-clamp-3");
    expect(toggle.textContent).toBe("more");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(body.id).toBeTruthy();
    expect(toggle.getAttribute("aria-controls")).toBe(body.id);
    fireEvent.click(toggle);
    expect(body.className).not.toContain("line-clamp-3");
    expect(toggle.textContent).toBe("less");
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(toggle);
    expect(body.className).toContain("line-clamp-3");
  });

  it("the pane footer renders outside the scroll area", () => {
    renderPane({
      blocks: [{ kind: "heading", text: "GitHub" }],
      footer: { text: "refreshed 12:07", value: "blocked", tone: "danger", icon: "refresh-cw" },
    });
    const footer = screen.getByTestId("plugin-pane-footer");
    expect(footer.textContent).toContain("refreshed 12:07");
    expect(footer.textContent).toContain("blocked");
    expect(footer.closest(".overflow-auto")).toBeNull();
  });
});
