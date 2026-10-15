// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { PluginUiEntry } from "../api";
import { PluginUiProvider, usePluginAttention } from "../pluginUiContext";

const { stateRef } = vi.hoisted(() => ({
  stateRef: { current: [] as PluginUiEntry[] },
}));
// Each render hands back freshly copied entries, as every plugin poll does.
vi.mock("../../hooks/usePluginUiState", () => ({
  usePluginUiState: () => ({
    entries: structuredClone(stateRef.current),
    revisions: {},
    isRefreshing: false,
    poke: () => {},
  }),
}));

const toned = (sessionId: string, tone: string): PluginUiEntry => ({
  plugin_id: "gh",
  slot: "row-column",
  id: "pr",
  session_id: sessionId,
  payload: { text: "PR", tone },
});

function mountAttention(entries: PluginUiEntry[]) {
  stateRef.current = entries;
  return renderHook(() => usePluginAttention(), { wrapper: PluginUiProvider });
}

describe("usePluginAttention (#4446)", () => {
  it("keeps its identity across polls that deliver the same membership", () => {
    const { result, rerender } = mountAttention([toned("a", "danger"), toned("b", "info")]);
    const first = result.current;
    expect([...first]).toEqual(["a"]);
    // Same toned sessions, different entries (a calm entry came and went).
    stateRef.current = [toned("a", "danger")];
    rerender();
    expect(result.current).toBe(first);
  });

  it("keeps the empty set across polls with no toned entries", () => {
    const { result, rerender } = mountAttention([]);
    const first = result.current;
    stateRef.current = [toned("a", "success")];
    rerender();
    expect(result.current).toBe(first);
  });

  it("changes when a tone appears and again when it clears", () => {
    const { result, rerender } = mountAttention([]);
    const none = result.current;

    stateRef.current = [toned("a", "warn")];
    rerender();
    const warned = result.current;
    expect(warned).not.toBe(none);
    expect([...warned]).toEqual(["a"]);

    stateRef.current = [toned("a", "warn"), toned("b", "danger")];
    rerender();
    expect([...result.current]).toEqual(["a", "b"]);

    stateRef.current = [];
    rerender();
    expect(result.current.size).toBe(0);
  });
});
