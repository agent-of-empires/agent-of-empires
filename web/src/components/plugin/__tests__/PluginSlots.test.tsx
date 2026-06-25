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
});
