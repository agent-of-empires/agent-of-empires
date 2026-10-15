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
  configureStorageSync(
    () => false,
    () => {},
  );
});

describe("isSyncedKey", () => {
  it("syncs preference keys but not device-local layout, caches, or per-session keys", () => {
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
      expect(isSyncedKey(k), k).toBe(true);
    }
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
      expect(isSyncedKey(k), k).toBe(false);
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

  it("server values win and local-only keys are not resurrected once the server is non-empty", async () => {
    localStorage.setItem("aoe-sidebar-axis", "repo");
    localStorage.setItem("aoe-welcome-seen", "1");
    getWebUiState.mockResolvedValue({ "aoe-sidebar-axis": "group" });

    await hydrateWebUiStateFromServer();

    expect(localStorage.getItem("aoe-sidebar-axis")).toBe("group");
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
