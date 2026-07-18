// @vitest-environment jsdom

import { fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { PluginCommand, PluginUiEntry } from "../../lib/api";

const { fetchMock, invokeMock } = vi.hoisted(() => ({
  fetchMock: vi.fn(),
  invokeMock: vi.fn(),
}));
vi.mock("../../lib/api", async (orig) => ({
  ...(await orig<typeof import("../../lib/api")>()),
  fetchPluginCommands: fetchMock,
  invokePluginCommand: invokeMock,
}));

import { usePluginCommands } from "../usePluginCommands";

const openPr: PluginCommand = {
  fqid: "plugin.acme.gh.open_pr",
  plugin_id: "acme.gh",
  id: "open_pr",
  title: "Open PR",
  description: "",
  keybinds: ["Ctrl+G"],
  action: { kind: "open-ui-link", slot: "detail-badge", id: "pr" },
};
const refresh: PluginCommand = {
  fqid: "plugin.acme.gh.refresh",
  plugin_id: "acme.gh",
  id: "refresh",
  title: "Refresh",
  description: "",
  keybinds: ["Ctrl+R"],
  action: null,
};

function badge(payload: Record<string, unknown>): PluginUiEntry {
  return { plugin_id: "acme.gh", slot: "detail-badge", id: "pr", session_id: "s1", payload };
}

const ctrl = (key: string) => ({ ctrlKey: true, shiftKey: false, altKey: false, metaKey: false, key });

beforeEach(() => {
  fetchMock.mockReset();
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(true);
});
afterEach(() => vi.restoreAllMocks());

describe("usePluginCommands keybinds", () => {
  it("opens the single resolved link on the open-ui-link chord", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    fetchMock.mockResolvedValue({ commands: [openPr] });
    const entries = [badge({ href: "https://example.com/pr/1" })];
    const { result } = renderHook(() => usePluginCommands(entries, "s1"));
    await waitFor(() => expect(result.current.actions.length).toBe(1));

    fireEvent.keyDown(document, ctrl("g"));
    expect(open).toHaveBeenCalledWith("https://example.com/pr/1", "_blank", "noopener,noreferrer");
    expect(result.current.overlay).toBeNull();
  });

  it("invokes the worker on an action-less chord", async () => {
    fetchMock.mockResolvedValue({ commands: [refresh] });
    const { result } = renderHook(() => usePluginCommands([], "s1"));
    // Action-less commands surface as palette actions once a session is active.
    await waitFor(() => expect(result.current.actions.length).toBe(1));

    fireEvent.keyDown(document, ctrl("r"));
    expect(invokeMock).toHaveBeenCalledWith("plugin.acme.gh.refresh", "s1");
  });

  it("shows the numbered picker overlay when the chord resolves to several links", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    fetchMock.mockResolvedValue({ commands: [openPr] });
    const entries = [
      badge({
        items: [
          { href: "https://example.com/pr/1", tooltip: "one" },
          { href: "https://example.com/pr/2", tooltip: "two" },
        ],
      }),
    ];
    const { result, rerender } = renderHook(() => usePluginCommands(entries, "s1"));
    await waitFor(() => expect(result.current.actions.length).toBe(2));

    fireEvent.keyDown(document, ctrl("g"));
    await waitFor(() => expect(result.current.overlay).not.toBeNull());

    // Rendering the overlay lists both links.
    rerender();
    render(result.current.overlay!);
    expect(screen.getByText("one")).toBeTruthy();
    expect(screen.getByText("two")).toBeTruthy();
  });

  it("ignores plugin chords typed into an input", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    fetchMock.mockResolvedValue({ commands: [openPr] });
    const entries = [badge({ href: "https://example.com/pr/1" })];
    renderHook(() => usePluginCommands(entries, "s1"));
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());

    const input = document.createElement("input");
    document.body.appendChild(input);
    fireEvent.keyDown(input, ctrl("g"));
    expect(open).not.toHaveBeenCalled();
    input.remove();
  });
});
