// @vitest-environment jsdom

import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginUiEntry } from "../../../lib/api";
import {
  PluginCards,
  PluginComposerActions,
  PluginPaneBody,
  PluginRowBadges,
  PluginSettingsPage,
  PluginStatusBarSegments,
  PluginToolCardBadges,
} from "../PluginSlots";
import { composerDraftOperation } from "../composerDraftOperation";

// The slot components read entries, the refresh flag, the per-plugin revision,
// and the poke fn from context; mock those hooks so each test drives a fixed
// snapshot and can advance the revision to simulate the poll seeing fresh state.
const { entriesRef, refreshingRef, revisionRef, pokeMock } = vi.hoisted(() => ({
  entriesRef: { current: [] as PluginUiEntry[] },
  refreshingRef: { current: false },
  revisionRef: { current: 0 },
  pokeMock: vi.fn(),
}));
vi.mock("../../../lib/pluginUiContext", () => ({
  usePluginUiEntries: () => entriesRef.current,
  usePluginUiRefreshing: () => refreshingRef.current,
  usePluginUiRevision: () => revisionRef.current,
  usePluginUiPoke: () => pokeMock,
}));

// The action block forwards to the worker via this; stub it. The default
// returns an accepted baseline of 0 (matching the initial revision).
const { invokeMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(async () => ({ baselineRevision: 0 })),
}));
vi.mock("../../../lib/api", () => ({ invokePluginAction: invokeMock }));

function set(entries: PluginUiEntry[]) {
  entriesRef.current = entries;
}

describe("plugin slot renderers", () => {
  beforeEach(() => {
    entriesRef.current = [];
    refreshingRef.current = false;
    revisionRef.current = 0;
    pokeMock.mockClear();
    invokeMock.mockReset();
    invokeMock.mockImplementation(async () => ({ baselineRevision: 0 }));
  });

  it("status-bar renders global segments and is empty otherwise", () => {
    set([]);
    const { container, rerender } = render(<PluginStatusBarSegments />);
    expect(container.textContent).toBe("");

    set([{ plugin_id: "acme.kit", slot: "status-bar", id: "s", payload: { text: "Build OK", tone: "success" } }]);
    rerender(<PluginStatusBarSegments />);
    expect(screen.getByText("Build OK")).toBeTruthy();
  });

  it("row-badge renders only the addressed session's entries", () => {
    set([
      { plugin_id: "acme.kit", slot: "row-badge", id: "b", session_id: "s1", payload: { text: "PR #12" } },
      { plugin_id: "acme.kit", slot: "row-badge", id: "b", session_id: "s2", payload: { text: "other" } },
    ]);
    render(<PluginRowBadges sessionId="s1" />);
    expect(screen.getByText("PR #12")).toBeTruthy();
    expect(screen.queryByText("other")).toBeNull();
  });

  it("row-badge with href renders a clickable link with a lucide icon", async () => {
    set([
      {
        plugin_id: "acme.kit",
        slot: "row-badge",
        id: "b",
        session_id: "s1",
        payload: {
          text: "PR #12",
          icon: "git-pull-request-arrow",
          href: "https://github.com/o/r/pull/12",
        },
      },
    ]);
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    const link = screen.getByRole("link", { name: /PR #12/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/12");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toContain("noopener");
    // The lucide icon lazy-loads (DynamicIcon) and renders as an inline svg.
    await waitFor(() => expect(container.querySelector("svg")).toBeTruthy());
  });

  it("row-badge with an unknown icon name renders text and no svg", () => {
    set([
      {
        plugin_id: "acme.kit",
        slot: "row-badge",
        id: "b",
        session_id: "s1",
        payload: { text: "plain", icon: "not-a-real-icon" },
      },
    ]);
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    expect(screen.getByText("plain")).toBeTruthy();
    expect(container.querySelector("svg")).toBeNull();
  });

  it("card renders title and body", () => {
    set([{ plugin_id: "acme.kit", slot: "card", id: "c", payload: { title: "Coverage", body: "92%" } }]);
    render(<PluginCards />);
    expect(screen.getByText("Coverage")).toBeTruthy();
    expect(screen.getByText("92%")).toBeTruthy();
  });

  it("pane action button forwards the named worker method", async () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "p",
      session_id: "s1",
      payload: { title: "GitHub", blocks: [{ kind: "action", label: "Refresh", method: "github.refresh" }] },
    };
    render(<PluginPaneBody entry={entry} />);
    const btn = screen.getByTestId("plugin-pane-action");
    expect(btn.textContent).toContain("Refresh");
    fireEvent.click(btn);
    // A block with no `params` still forwards an empty object, which is what the
    // API sends on the wire either way.
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("acme.kit", "github.refresh", "s1", {}));
  });

  it("composer action button forwards a composer snapshot to the worker", async () => {
    set([
      {
        plugin_id: "acme.voice",
        slot: "composer-action",
        id: "dictate",
        session_id: "s1",
        payload: { label: "Voice", method: "voice.start", icon: "mic" },
      },
    ]);
    const getSnapshot = vi.fn(() => ({ text: "hello", selectionStart: 1, selectionEnd: 5 }));
    render(<PluginComposerActions sessionId="s1" getSnapshot={getSnapshot} />);

    fireEvent.click(screen.getByTestId("plugin-composer-action"));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("acme.voice", "voice.start", "s1", {
        composer: { text: "hello", selection_start: 1, selection_end: 5 },
      }),
    );
    expect(pokeMock).toHaveBeenCalled();
  });

  it("composer action parses valid draft operations", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.voice",
      slot: "composer-action",
      id: "dictate",
      session_id: "s1",
      payload: {
        label: "Voice",
        method: "voice.start",
        draft_operation: { kind: "insert-text", id: "op-1", text: "hello" },
      },
    };
    expect(composerDraftOperation(entry)).toEqual({
      id: "op-1",
      operation: { kind: "insert-text", text: "hello" },
    });
    expect(
      composerDraftOperation({
        ...entry,
        payload: { ...entry.payload, draft_operation: { kind: "set-text", id: "op-2", text: "" } },
      }),
    ).toEqual({
      id: "op-2",
      operation: { kind: "set-text", text: "" },
    });
    expect(
      composerDraftOperation({
        ...entry,
        payload: { ...entry.payload, draft_operation: { kind: "bad", id: "op-2", text: "hello" } },
      }),
    ).toBeNull();
  });

  it("holds the spinner until the plugin revision advances, not just until the POST resolves", async () => {
    // The host had revision 7 when it accepted the action; the worker re-pushes
    // its state asynchronously, bumping the revision to 8 on a later poll.
    revisionRef.current = 7;
    invokeMock.mockImplementationOnce(async () => ({ baselineRevision: 7 }));
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "p",
      session_id: "s1",
      payload: { blocks: [{ kind: "action", label: "Refresh", method: "github.refresh" }] },
    };
    const { container, rerender } = render(<PluginPaneBody entry={entry} />);
    const btn = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;

    fireEvent.click(btn);
    // POST has resolved, but the revision has not moved yet: the spinner must
    // stay (the old behavior cleared it here, which is the bug).
    await waitFor(() => expect(pokeMock).toHaveBeenCalled());
    expect(container.querySelector("svg.animate-spin")).toBeTruthy();
    expect(btn.getAttribute("aria-busy")).toBe("true");
    expect(btn.disabled).toBe(true);

    // The poll delivers the worker's re-pushed state: revision moves off the
    // baseline and the spinner clears.
    revisionRef.current = 8;
    rerender(<PluginPaneBody entry={entry} />);
    await waitFor(() => expect(container.querySelector("svg.animate-spin")).toBeNull());
    expect(btn.getAttribute("aria-busy")).toBeNull();
    expect(btn.disabled).toBe(false);
  });

  it("clears a stuck spinner after the timeout when no fresh state arrives", async () => {
    vi.useFakeTimers();
    try {
      revisionRef.current = 3;
      invokeMock.mockImplementationOnce(async () => ({ baselineRevision: 3 }));
      const entry: PluginUiEntry = {
        plugin_id: "acme.kit",
        slot: "pane",
        id: "p",
        session_id: "s1",
        payload: { blocks: [{ kind: "action", label: "Refresh", method: "github.refresh" }] },
      };
      const { container } = render(<PluginPaneBody entry={entry} />);
      const btn = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;

      await act(async () => {
        fireEvent.click(btn);
      });
      expect(container.querySelector("svg.animate-spin")).toBeTruthy();

      // Revision never moves; the hard timeout restores the button.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15000);
      });
      expect(container.querySelector("svg.animate-spin")).toBeNull();
      expect(btn.disabled).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("skips the wait and clears on POST settle when the daemon omits a baseline", async () => {
    // Older daemon: no baseline_revision, so the API returns null. The button
    // must not spin to the 15s timeout; it clears once the POST settles.
    revisionRef.current = 0;
    invokeMock.mockImplementationOnce(async () => ({ baselineRevision: null }));
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "p",
      session_id: "s1",
      payload: { blocks: [{ kind: "action", label: "Refresh", method: "github.refresh" }] },
    };
    const { container } = render(<PluginPaneBody entry={entry} />);
    const btn = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;
    fireEvent.click(btn);
    await waitFor(() => expect(pokeMock).toHaveBeenCalled());
    await waitFor(() => {
      expect(container.querySelector("svg.animate-spin")).toBeNull();
      expect(btn.disabled).toBe(false);
    });
  });

  it("pane action stops the spinner and stays actionable when the POST fails", async () => {
    invokeMock.mockImplementationOnce(async () => null);
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "p",
      session_id: "s1",
      payload: { blocks: [{ kind: "action", label: "Refresh", method: "github.refresh" }] },
    };
    const { container } = render(<PluginPaneBody entry={entry} />);
    const btn = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;
    fireEvent.click(btn);
    await waitFor(() => expect(invokeMock).toHaveBeenCalled());
    await waitFor(() => {
      expect(container.querySelector("svg.animate-spin")).toBeNull();
      expect(btn.disabled).toBe(false);
    });
  });

  it("pane shows a background-refresh indicator only while a poll is in flight", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "p",
      session_id: "s1",
      payload: { title: "GitHub", body: "ok" },
    };
    refreshingRef.current = true;
    const { rerender } = render(<PluginPaneBody entry={entry} />);
    expect(screen.getByTestId("plugin-pane-refreshing")).toBeTruthy();

    refreshingRef.current = false;
    rerender(<PluginPaneBody entry={{ ...entry, payload: { ...entry.payload } }} />);
    expect(screen.queryByTestId("plugin-pane-refreshing")).toBeNull();
  });

  it("pane action block without a method renders nothing", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "p",
      session_id: "s1",
      payload: { blocks: [{ kind: "action", label: "Refresh" }] },
    };
    render(<PluginPaneBody entry={entry} />);
    expect(screen.queryByTestId("plugin-pane-action")).toBeNull();
  });

  it("pane renders its title/body", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "p",
      session_id: "s1",
      payload: { title: "Logs", body: "tail..." },
    };
    render(<PluginPaneBody entry={entry} />);
    expect(screen.getByText("Logs")).toBeTruthy();
    expect(screen.getByText("tail...")).toBeTruthy();
  });

  it("row-badge items render one clickable icon per item", async () => {
    set([
      {
        plugin_id: "acme.kit",
        slot: "row-badge",
        id: "repos",
        session_id: "s1",
        payload: {
          items: [
            { icon: "git-pull-request-arrow", tone: "success", href: "https://x/pr/1", tooltip: "PR #1" },
            { icon: "git-pull-request-draft", tone: "warn", href: "https://x/pr/2", tooltip: "PR #2" },
          ],
        },
      },
    ]);
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    const links = screen.getAllByRole("link");
    expect(links).toHaveLength(2);
    expect(links[0]!.getAttribute("href")).toBe("https://x/pr/1");
    expect(links[1]!.getAttribute("rel")).toContain("noopener");
    await waitFor(() => expect(container.querySelectorAll("svg")).toHaveLength(2));
    // Icon-only links must carry an accessible name from the tooltip.
    expect(screen.getByRole("link", { name: "PR #1" })).toBeTruthy();
    // Icon-only badges size to the icon: no text truncation (which clipped the
    // icon), and shrink-0 so the row's flex cannot squeeze them.
    for (const link of links) {
      expect(link.className).not.toContain("truncate");
      expect(link.className).toContain("shrink-0");
    }
  });

  it("row-badge empty items clears the row (renders nothing)", () => {
    set([{ plugin_id: "acme.kit", slot: "row-badge", id: "repos", session_id: "s1", payload: { items: [] } }]);
    const { container } = render(<PluginRowBadges sessionId="s1" />);
    expect(container.querySelector("a, span")).toBeNull();
  });

  it("row-badge item with a non-http href is not a link", () => {
    set([
      {
        plugin_id: "acme.kit",
        slot: "row-badge",
        id: "repos",
        session_id: "s1",
        payload: { items: [{ text: "evil", href: "javascript:alert(1)" }] },
      },
    ]);
    render(<PluginRowBadges sessionId="s1" />);
    expect(screen.queryByRole("link")).toBeNull();
    expect(screen.getByText("evil")).toBeTruthy();
  });

  it("pane blocks render heading, row, note, divider and skip unknown kinds", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "gh",
      session_id: "s1",
      payload: {
        blocks: [
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
        ],
      },
    };
    const { container } = render(<PluginPaneBody entry={entry} />);
    expect(screen.getByText("GitHub")).toBeTruthy();
    expect(screen.getByText("nexus")).toBeTruthy();
    expect(screen.getByText("3 repos have no open PR")).toBeTruthy();
    // The row with an href is an anchor; the unknown kind contributed nothing.
    const link = screen.getByRole("link", { name: /nexus/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/nexus/pull/12");
    expect(container.querySelector("hr")).toBeTruthy();
  });

  it("a row with a validated hex color tints via inline style; junk is ignored", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "gh",
      session_id: "s1",
      payload: {
        blocks: [
          { kind: "row", icon: "git-merge", label: "nexus", value: "MERGED #12", color: "#8957e5" },
          { kind: "row", label: "other", value: "open", color: "javascript:alert(1)" },
        ],
      },
    };
    render(<PluginPaneBody entry={entry} />);
    // jsdom normalizes the hex to rgb when it lands on the style attribute.
    const merged = screen.getByText("MERGED #12");
    expect(merged.style.color).toBe("rgb(137, 87, 229)");
    // An invalid color leaves the value untinted (no inline color style).
    const other = screen.getByText("open");
    expect(other.style.color).toBe("");
  });

  it("a collapsible section renders a foldable details; collapsed sets the initial state", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "gh",
      session_id: "s1",
      payload: {
        blocks: [
          { kind: "section", title: "Checks: passing", collapsible: true, children: [{ kind: "note", text: "ci" }] },
          {
            kind: "section",
            title: "Unresolved comments: 2",
            collapsible: true,
            collapsed: true,
            children: [{ kind: "note", text: "cmt" }],
          },
          { kind: "section", title: "Plain", children: [{ kind: "note", text: "x" }] },
        ],
      },
    };
    const { container } = render(<PluginPaneBody entry={entry} />);
    const details = container.querySelectorAll("details");
    expect(details).toHaveLength(2);
    // First (no `collapsed`) starts open; second (collapsed:true) starts closed.
    expect((details[0] as HTMLDetailsElement).open).toBe(true);
    expect((details[1] as HTMLDetailsElement).open).toBe(false);
    // The title and children live inside the disclosure.
    expect(screen.getByText("Checks: passing")).toBeTruthy();
    expect(screen.getByText("cmt")).toBeTruthy();
    // A section without the flag stays a plain <section>, not a <details>.
    expect(container.querySelector("section")).toBeTruthy();
  });

  it("a collapsible section keeps the user's fold across a re-push (uncontrolled)", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "gh",
      session_id: "s1",
      payload: {
        blocks: [{ kind: "section", title: "Checks", collapsible: true, children: [{ kind: "note", text: "ci" }] }],
      },
    };
    const { container, rerender } = render(<PluginPaneBody entry={entry} />);
    const details = container.querySelector("details") as HTMLDetailsElement;
    expect(details.open).toBe(true);
    // User folds it shut. The worker re-pushes the same pane state (a new object
    // each poll); a controlled `open` would snap it back open.
    details.open = false;
    rerender(<PluginPaneBody entry={{ ...entry, payload: { ...entry.payload } }} />);
    expect((container.querySelector("details") as HTMLDetailsElement).open).toBe(false);
  });

  it("a section title renders a tone-tinted icon for at-a-glance status", async () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "gh",
      session_id: "s1",
      payload: {
        blocks: [
          {
            kind: "section",
            title: "Checks: passing",
            collapsible: true,
            collapsed: true,
            icon: "circle-check",
            tone: "success",
            children: [{ kind: "note", text: "ci" }],
          },
        ],
      },
    };
    const { container } = render(<PluginPaneBody entry={entry} />);
    const summary = container.querySelector("summary")!;
    // The success tone tints the title text, visible even when folded.
    expect(summary.className).toContain("text-status-running");
    // Both the chevron and the lazy-loaded status icon render as svgs.
    await waitFor(() => expect(summary.querySelectorAll("svg")).toHaveLength(2));
  });

  it("comment blocks render read-only with author, location and resolved state", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "gh",
      session_id: "s1",
      payload: {
        blocks: [
          {
            kind: "section",
            title: "Unresolved comments: 1",
            children: [
              {
                kind: "comment",
                author: "alice",
                body: "handle the nil case",
                path: "src/foo.py",
                line: 42,
                href: "https://github.com/o/r/pull/1#c1",
                resolved: false,
              },
            ],
          },
        ],
      },
    };
    render(<PluginPaneBody entry={entry} />);
    expect(screen.getByText("alice")).toBeTruthy();
    expect(screen.getByText("handle the nil case")).toBeTruthy();
    expect(screen.getByText("src/foo.py:42")).toBeTruthy();
    expect(screen.getByText("unresolved")).toBeTruthy();
    const link = screen.getByRole("link");
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/1#c1");
    // Read-only: no reply/resolve controls, and a short body needs no toggle.
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.queryByTestId("plugin-comment-toggle")).toBeNull();
  });

  it("a long comment body is clamped with a more/less toggle", () => {
    const longBody = "x".repeat(250);
    const entry: PluginUiEntry = {
      plugin_id: "acme.kit",
      slot: "pane",
      id: "gh",
      session_id: "s1",
      payload: { blocks: [{ kind: "comment", author: "bob", body: longBody }] },
    };
    render(<PluginPaneBody entry={entry} />);
    const body = screen.getByText(longBody);
    expect(body.className).toContain("line-clamp-3");
    const toggle = screen.getByTestId("plugin-comment-toggle");
    expect(toggle.textContent).toBe("more");
    // Toggle state and the controlled body are exposed to assistive tech.
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(toggle.getAttribute("aria-controls")).toBe(body.id);
    expect(body.id).toBeTruthy();
    fireEvent.click(toggle);
    expect(body.className).not.toContain("line-clamp-3");
    expect(toggle.textContent).toBe("less");
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(toggle);
    expect(body.className).toContain("line-clamp-3");
  });

  it("settings-page shows a waiting state until its global entry is pushed", () => {
    // Nav appears on declaration, so before the worker pushes anything the page
    // is empty: render an explicit waiting state, not a blank page.
    set([]);
    const { rerender } = render(<PluginSettingsPage pluginId="acme.mcp" contribId="servers" pluginName="MCP" />);
    expect(screen.getByTestId("plugin-settings-page-waiting")).toBeTruthy();
    expect(screen.getByText(/Waiting for MCP/)).toBeTruthy();

    // Once the plugin pushes its global (session-less) settings-page entry, the
    // page body renders through the shared block vocabulary.
    set([
      {
        plugin_id: "acme.mcp",
        slot: "settings-page",
        id: "servers",
        payload: { blocks: [{ kind: "heading", text: "Servers" }] },
      },
    ]);
    rerender(<PluginSettingsPage pluginId="acme.mcp" contribId="servers" pluginName="MCP" />);
    expect(screen.queryByTestId("plugin-settings-page-waiting")).toBeNull();
    expect(screen.getByTestId("plugin-settings-page")).toBeTruthy();
    expect(screen.getByText("Servers")).toBeTruthy();
  });

  it("settings-page selects only the matching (plugin_id, id) global entry", () => {
    // A different plugin's or contribution's entry must not fill this page, and
    // a per-session entry (session_id set) is never a settings-page match.
    set([
      { plugin_id: "other.kit", slot: "settings-page", id: "servers", payload: { title: "Other" } },
      {
        plugin_id: "acme.mcp",
        slot: "settings-page",
        id: "other",
        payload: { title: "Wrong page" },
      },
      {
        plugin_id: "acme.mcp",
        slot: "settings-page",
        id: "servers",
        session_id: "s1",
        payload: { title: "Scoped" },
      },
    ]);
    render(<PluginSettingsPage pluginId="acme.mcp" contribId="servers" pluginName="MCP" />);
    expect(screen.getByTestId("plugin-settings-page-waiting")).toBeTruthy();
    expect(screen.queryByText("Other")).toBeNull();
    expect(screen.queryByText("Wrong page")).toBeNull();
    expect(screen.queryByText("Scoped")).toBeNull();
  });
});

describe("tool-card-badge renderer", () => {
  beforeEach(() => {
    entriesRef.current = [];
  });

  const badge = (target: { kind: string; name: string }, text: string): PluginUiEntry => ({
    plugin_id: "acme.prov",
    slot: "tool-card-badge",
    id: "provenance",
    session_id: "s1",
    payload: { items: [{ target, text }] },
  });

  it("renders the pill whose target matches the card kind and name", () => {
    set([badge({ kind: "mcp", name: "github" }, "MCP")]);
    render(<PluginToolCardBadges sessionId="s1" kind="mcp" target="github" />);
    expect(screen.getByText("MCP")).toBeTruthy();
  });

  it("ignores badges whose target name differs", () => {
    set([badge({ kind: "mcp", name: "github" }, "MCP")]);
    const { container } = render(<PluginToolCardBadges sessionId="s1" kind="mcp" target="gitlab" />);
    expect(container.textContent).toBe("");
  });

  it("does not cross-render an mcp badge onto a same-named skill card", () => {
    set([badge({ kind: "mcp", name: "deploy" }, "from-mcp")]);
    const { container } = render(<PluginToolCardBadges sessionId="s1" kind="skill" target="deploy" />);
    expect(container.textContent).toBe("");
  });

  it("renders only the addressed session's badges", () => {
    set([
      { ...badge({ kind: "mcp", name: "github" }, "mine"), session_id: "s1" },
      { ...badge({ kind: "mcp", name: "github" }, "other"), session_id: "s2" },
    ]);
    render(<PluginToolCardBadges sessionId="s1" kind="mcp" target="github" />);
    expect(screen.getByText("mine")).toBeTruthy();
    expect(screen.queryByText("other")).toBeNull();
  });
});

// The block kinds and fields added in api_version 12: callout, bar, columns, the
// pane footer, and the interactive/summary additions to row, section and action.
describe("pane block vocabulary (api 12)", () => {
  beforeEach(() => {
    entriesRef.current = [];
    refreshingRef.current = false;
    revisionRef.current = 0;
    pokeMock.mockClear();
    invokeMock.mockReset();
    invokeMock.mockImplementation(async () => ({ baselineRevision: 0 }));
  });

  function pane(payload: Record<string, unknown>): PluginUiEntry {
    return { plugin_id: "acme.kit", slot: "pane", id: "gh", session_id: "s1", payload };
  }

  it("a callout renders its verdict and stretches its actions; a disabled action never posts", () => {
    render(
      <PluginPaneBody
        entry={pane({
          blocks: [
            {
              kind: "callout",
              tone: "danger",
              icon: "circle-x",
              title: "2 required checks failing",
              detail: "Merging is blocked until Clippy passes.",
              actions: [{ kind: "action", label: "Merge blocked", method: "gh.merge", disabled: true }],
            },
          ],
        })}
      />,
    );
    expect(screen.getByTestId("plugin-pane-callout")).toBeTruthy();
    expect(screen.getByText("2 required checks failing")).toBeTruthy();
    expect(screen.getByText("Merging is blocked until Clippy passes.")).toBeTruthy();
    const button = screen.getByTestId("plugin-pane-action") as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.click(button);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("a callout with neither title nor detail renders nothing", () => {
    const { container } = render(<PluginPaneBody entry={pane({ blocks: [{ kind: "callout", tone: "danger" }] })} />);
    expect(container.querySelector("[data-testid='plugin-pane-callout']")).toBeNull();
  });

  it("an action with an href and no method links out instead of posting", () => {
    render(
      <PluginPaneBody
        entry={pane({
          blocks: [
            { kind: "action", label: "Squash and merge", href: "https://github.com/o/r/pull/1", variant: "primary" },
          ],
        })}
      />,
    );
    const link = screen.getByRole("link", { name: /Squash and merge/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/1");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("a bar sizes segments proportionally and drops non-positive values", () => {
    const { container } = render(
      <PluginPaneBody
        entry={pane({
          blocks: [
            {
              kind: "bar",
              caption: "18 files",
              segments: [
                { value: 750, tone: "success" },
                { value: 250, tone: "danger" },
                { value: 0, tone: "warn" },
                { tone: "info" },
              ],
            },
          ],
        })}
      />,
    );
    // Scoped to the track, so the caption span below it is not counted.
    const spans = Array.from(container.querySelectorAll("[data-testid='plugin-pane-bar'] > div > span"));
    // Only the two positive segments survive, at 75% / 25% of the total.
    expect(spans.map((s) => (s as HTMLElement).style.width)).toEqual(["75%", "25%"]);
    expect(screen.getByText("18 files")).toBeTruthy();
  });

  it("a bar with no positive segment renders nothing", () => {
    const { container } = render(
      <PluginPaneBody entry={pane({ blocks: [{ kind: "bar", segments: [{ value: 0 }] }] })} />,
    );
    expect(container.querySelector("[data-testid='plugin-pane-bar']")).toBeNull();
  });

  it("columns lay children side by side, and a lone child spans the full width", () => {
    const two = render(
      <PluginPaneBody
        entry={pane({
          blocks: [
            {
              kind: "columns",
              children: [
                { kind: "section", title: "DIFF", children: [{ kind: "row", value: "+842 -317" }] },
                {
                  kind: "section",
                  title: "LINKED",
                  children: [{ kind: "row", prefix: "#3180", label: "Stale daemon" }],
                },
              ],
            },
          ],
        })}
      />,
    );
    expect(two.getByTestId("plugin-pane-columns").className).toContain("grid-cols-2");
    expect(screen.getByText("Stale daemon")).toBeTruthy();
    two.unmount();

    const one = render(
      <PluginPaneBody
        entry={pane({ blocks: [{ kind: "columns", children: [{ kind: "section", title: "DIFF" }] }] })}
      />,
    );
    expect(one.getByTestId("plugin-pane-columns").className).toContain("grid-cols-1");
  });

  it("columns with no children render nothing", () => {
    const { container } = render(<PluginPaneBody entry={pane({ blocks: [{ kind: "columns", children: [] }] })} />);
    expect(container.querySelector("[data-testid='plugin-pane-columns']")).toBeNull();
  });

  it("a row with a method posts it, marks selection, and keeps its href as a separate link", async () => {
    render(
      <PluginPaneBody
        entry={pane({
          blocks: [
            {
              kind: "row",
              prefix: "#3231",
              label: "fix(update): warn when daemon is stale",
              sublabel: "japanese · njbrake",
              method: "gh.select_pr",
              params: { repo: "o/r", number: 3231 },
              selected: true,
              href: "https://github.com/o/r/pull/3231",
              badges: [{ icon: "circle-x", tone: "danger", tooltip: "CI failing" }],
            },
          ],
        })}
      />,
    );
    // The body is the selectable control; the href is a sibling link, so the
    // row can be picked without navigating away.
    const button = screen.getByRole("button", { name: /fix\(update\)/ });
    expect(button.getAttribute("aria-pressed")).toBe("true");
    const link = screen.getByRole("link", { name: /Open .* externally/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/3231");
    // The glyph-only badge is named from its tooltip so it is not silent.
    expect(screen.getByLabelText("CI failing")).toBeTruthy();

    await act(async () => {
      fireEvent.click(button);
    });
    // `params` names the subject, so one method can serve every row.
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("acme.kit", "gh.select_pr", "s1", { repo: "o/r", number: 3231 }),
    );
  });

  it("a row with only a prefix still renders, and the value pins right of the label", () => {
    render(
      <PluginPaneBody
        entry={pane({
          blocks: [
            { kind: "row", prefix: "◉", tone: "success" },
            { kind: "row", label: "Nate Brake", value: "approved", avatar: "NB" },
          ],
        })}
      />,
    );
    expect(screen.getByText("◉")).toBeTruthy();
    expect(screen.getByText("NB")).toBeTruthy();
    expect(screen.getByText("approved").className).toContain("ml-auto");
  });

  it("a section header pins a value summary and badge pills", () => {
    render(
      <PluginPaneBody
        entry={pane({
          blocks: [
            {
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
            },
          ],
        })}
      />,
    );
    expect(screen.getByText("1 of 2 approved")).toBeTruthy();
    expect(screen.getByText("2 failing")).toBeTruthy();
    expect(screen.getByText("17 passing")).toBeTruthy();
    expect(screen.getByText("Clippy")).toBeTruthy();
  });

  it("the pane footer renders outside the scroll area, and an empty one renders nothing", () => {
    const withFooter = render(
      <PluginPaneBody
        entry={pane({
          blocks: [{ kind: "heading", text: "GitHub" }],
          footer: { text: "refreshed 12:07", value: "blocked", tone: "danger", icon: "refresh-cw" },
        })}
      />,
    );
    const footer = withFooter.getByTestId("plugin-pane-footer");
    expect(footer.textContent).toContain("refreshed 12:07");
    expect(footer.textContent).toContain("blocked");
    // Pinned: a sibling of the scrolling block list, never inside it.
    expect(footer.closest(".overflow-auto")).toBeNull();
    withFooter.unmount();

    const empty = render(
      <PluginPaneBody entry={pane({ blocks: [{ kind: "heading", text: "GitHub" }], footer: {} })} />,
    );
    expect(empty.container.querySelector("[data-testid='plugin-pane-footer']")).toBeNull();
  });
});
