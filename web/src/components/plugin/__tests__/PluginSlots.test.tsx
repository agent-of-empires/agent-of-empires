// @vitest-environment jsdom

import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { PluginUiEntry } from "../../../lib/api";
import { PluginCards, PluginDetailPanels, PluginRowBadges, PluginStatusBarSegments } from "../PluginSlots";

// The slot components read entries from context; mock that hook so each test
// drives a fixed snapshot.
const { entriesRef } = vi.hoisted(() => ({ entriesRef: { current: [] as PluginUiEntry[] } }));
vi.mock("../../../lib/pluginUiContext", () => ({
  usePluginUiEntries: () => entriesRef.current,
}));

function set(entries: PluginUiEntry[]) {
  entriesRef.current = entries;
}

describe("plugin slot renderers", () => {
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

  it("row-badge with href renders a clickable link with a lucide icon", () => {
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
    // The lucide icon renders as an inline svg.
    expect(container.querySelector("svg")).toBeTruthy();
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

  it("detail-panel renders for its session", () => {
    set([
      {
        plugin_id: "acme.kit",
        slot: "detail-panel",
        id: "p",
        session_id: "s1",
        payload: { title: "Logs", body: "tail..." },
      },
    ]);
    render(<PluginDetailPanels sessionId="s1" />);
    expect(screen.getByText("Logs")).toBeTruthy();
    expect(screen.getByText("tail...")).toBeTruthy();
  });

  it("row-badge items render one clickable icon per item", () => {
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
    expect(container.querySelectorAll("svg")).toHaveLength(2);
    // Icon-only links must carry an accessible name from the tooltip.
    expect(screen.getByRole("link", { name: "PR #1" })).toBeTruthy();
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

  it("detail-panel blocks render heading, row, note, divider and skip unknown kinds", () => {
    set([
      {
        plugin_id: "acme.kit",
        slot: "detail-panel",
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
      },
    ]);
    const { container } = render(<PluginDetailPanels sessionId="s1" />);
    expect(screen.getByText("GitHub")).toBeTruthy();
    expect(screen.getByText("nexus")).toBeTruthy();
    expect(screen.getByText("3 repos have no open PR")).toBeTruthy();
    // The row with an href is an anchor; the unknown kind contributed nothing.
    const link = screen.getByRole("link", { name: /nexus/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/nexus/pull/12");
    expect(container.querySelector("hr")).toBeTruthy();
  });
});
