// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const getWebUiState = vi.fn();
const patchWebUiState = vi.fn();
vi.mock("../api", () => ({
  getWebUiState: (...args: unknown[]) => getWebUiState(...args),
  patchWebUiState: (...args: unknown[]) => patchWebUiState(...args),
}));

import { hydrateWebUiStateFromServer, isSyncedKey } from "../webUiSync";
import { configureStorageSync, safeRemoveItem, safeSetItem } from "../safeStorage";

beforeEach(() => {
  localStorage.clear();
  getWebUiState.mockReset();
  patchWebUiState.mockReset().mockResolvedValue(true);
});

afterEach(() => {
  // Reset the safeStorage chokepoint so tests don't leak the matcher/handler.
  configureStorageSync(
    () => false,
    () => {},
  );
});

describe("isSyncedKey", () => {
  it("matches the synced preference keys (exact + dynamic group prefixes)", () => {
    for (const k of [
      "aoe-welcome-seen",
      "aoe.acp.toolDensity.v1",
      "aoe-sidebar-sort-mode",
      "aoe-sidebar-axis",
      "aoe-sidebar-sunk-expanded",
      "aoe-repo-appearance-v1",
      "aoe-repo-group-order-v1",
      "aoe-acp-last-tool",
      "aoe-last-browse-dir",
      "aoe-web-settings",
      "aoe-repo-collapsed-/home/me/repo",
      "aoe-group-collapsed-feature",
      "aoe-nested-group-collapsed-x",
    ]) {
      expect(isSyncedKey(k)).toBe(true);
    }
  });

  it("excludes device-local layout, caches, and per-session keys", () => {
    for (const k of [
      "aoe-sidebar-width",
      "aoe-split-ratio",
      "aoe-right-vsplit",
      "aoe-pane-layout",
      "aoe-resolved-theme",
      "aoe-tour-seen",
      "aoe:acp-state:v1:abc",
      "aoe-acp-draft-abc",
      "unrelated",
    ]) {
      expect(isSyncedKey(k)).toBe(false);
    }
  });
});

describe("safeStorage sync chokepoint", () => {
  it("invokes the handler only for matched keys, with null on removal", () => {
    const handler = vi.fn();
    configureStorageSync(isSyncedKey, handler);

    safeSetItem("aoe-sidebar-axis", "group");
    expect(handler).toHaveBeenCalledWith("aoe-sidebar-axis", "group");

    handler.mockClear();
    safeSetItem("aoe-sidebar-width", "300"); // device-local, not synced
    expect(handler).not.toHaveBeenCalled();

    safeRemoveItem("aoe-welcome-seen");
    expect(handler).toHaveBeenCalledWith("aoe-welcome-seen", null);
  });
});

describe("hydrateWebUiStateFromServer", () => {
  it("backfills local keys only when the server blob is empty (first-ever sync)", async () => {
    localStorage.setItem("aoe-welcome-seen", "1");
    getWebUiState.mockResolvedValue({});

    await hydrateWebUiStateFromServer();

    expect(patchWebUiState).toHaveBeenCalledTimes(1);
    expect(patchWebUiState).toHaveBeenCalledWith({ "aoe-welcome-seen": "1" });
  });

  it("does NOT backfill (resurrect) local-only keys once the server is non-empty", async () => {
    // The server has data (already synced once). A local-only key here can mean
    // "deleted on another device", so it must not be pushed back up.
    localStorage.setItem("aoe-welcome-seen", "1");
    getWebUiState.mockResolvedValue({ "aoe-sidebar-axis": "group" });

    await hydrateWebUiStateFromServer();

    // Server value applied; no backfill.
    expect(localStorage.getItem("aoe-sidebar-axis")).toBe("group");
    expect(patchWebUiState).not.toHaveBeenCalled();
  });

  it("server values win over a differing local value", async () => {
    localStorage.setItem("aoe-sidebar-axis", "repo");
    getWebUiState.mockResolvedValue({ "aoe-sidebar-axis": "group" });

    await hydrateWebUiStateFromServer();

    expect(localStorage.getItem("aoe-sidebar-axis")).toBe("group");
    // Nothing to backfill: the only synced key is already on the server.
    expect(patchWebUiState).not.toHaveBeenCalled();
  });

  it("leaves the local cache untouched when the fetch fails", async () => {
    localStorage.setItem("aoe-sidebar-axis", "repo");
    getWebUiState.mockResolvedValue(null);

    await hydrateWebUiStateFromServer();

    expect(localStorage.getItem("aoe-sidebar-axis")).toBe("repo");
    expect(patchWebUiState).not.toHaveBeenCalled();
  });
});
