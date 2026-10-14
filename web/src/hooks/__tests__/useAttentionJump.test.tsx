// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { PluginUiEntry } from "../../lib/api";
import { PluginUiProvider } from "../../lib/pluginUiContext";
import { repoGroupToSidebarGroup } from "../../lib/sidebarGroups";
import { nextAttentionSessionId } from "../../lib/sidebarSort";
import { repoGroup, session, workspace } from "./fixtures";
import { useAttentionJump } from "../useAttentionJump";

const { entriesRef } = vi.hoisted(() => ({
  entriesRef: { current: [] as PluginUiEntry[] },
}));
vi.mock("../usePluginUiState", () => ({
  usePluginUiState: () => ({
    entries: entriesRef.current,
    revisions: {},
    isRefreshing: false,
    poke: () => {},
  }),
}));

const ws = (id: string, over = {}) => workspace(`w-${id}`, "/repo", [session({ id, ...over })]);
const groups = [
  repoGroupToSidebarGroup(
    repoGroup({
      workspaces: [ws("a"), ws("b"), ws("c", { snoozed_until: "2099-01-01T00:00:00Z" })],
    }),
  ),
];
const toned = (sessionId: string, tone: string): PluginUiEntry => ({
  plugin_id: "gh",
  slot: "row-column",
  id: "pr",
  session_id: sessionId,
  payload: { text: "PR", tone },
});

function mountJump() {
  return renderHook(() => useAttentionJump(groups), {
    wrapper: PluginUiProvider,
  });
}

describe("useAttentionJump (#4446)", () => {
  it("lists sessions in display order and leaves calm sessions out of the attention set", () => {
    entriesRef.current = [toned("b", "info")];
    const { orderedIds, attention } = mountJump().result.current;
    expect(orderedIds).toEqual(["a", "b", "c"]);
    expect(attention.size).toBe(0);
    expect(nextAttentionSessionId(orderedIds, attention, "a")).toBeNull();
  });

  it("jumps to a session carrying a plugin danger or warn tone, skipping sunk ones", () => {
    entriesRef.current = [toned("b", "danger"), toned("c", "warn")];
    const { orderedIds, attention } = mountJump().result.current;
    expect([...attention]).toEqual(["b"]);
    expect(nextAttentionSessionId(orderedIds, attention, "a")).toBe("b");
  });

  it("follows a tone appearing and clearing between polls", () => {
    entriesRef.current = [];
    const { result, rerender } = mountJump();
    expect(result.current.attention.size).toBe(0);

    entriesRef.current = [toned("a", "danger")];
    rerender();
    expect([...result.current.attention]).toEqual(["a"]);

    entriesRef.current = [];
    rerender();
    expect(result.current.attention.size).toBe(0);
  });

  it("keeps the result across polls that change nothing about attention", () => {
    entriesRef.current = [toned("a", "danger")];
    const { result, rerender } = mountJump();
    const first = result.current;
    entriesRef.current = [toned("a", "danger")];
    rerender();
    expect(result.current).toBe(first);
  });
});
