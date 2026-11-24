// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const getWebUiState = vi.fn();
const patchWebUiState = vi.fn();
vi.mock("../api", () => ({
  getWebUiState: (...args: unknown[]) => getWebUiState(...args),
  patchWebUiState: (...args: unknown[]) => patchWebUiState(...args),
}));

import { hydrateWebUiStateFromServer, initWebUiSync, isSyncedKey } from "../webUiSync";
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
      "aoe.acp.toolDensity.v1",
      "aoe-sidebar-sunk-expanded",
      "aoe-sidebar-suggested-expanded",
      "aoe-acp-last-tool",
      "aoe-last-browse-dir",
      "aoe-web-settings",
      "aoe-suggestions-hint-dismissed",
      "aoe-repo-collapsed-/home/me/repo",
      "aoe-group-collapsed-feature",
      "aoe-nested-group-collapsed-x",
      "aoe-settings-intro-worktree", // folded Settings tab intro
    ]) {
      expect(isSyncedKey(k), k).toBe(true);
    }
    for (const k of [
      "aoe-local-settings-intro-worktree", // a read-only dashboard's fold stays on the device
      // The grouping, sort and repository order are the Daemon's now.
      "aoe-sidebar-sort-mode",
      "aoe-sidebar-axis",
      "aoe-repo-group-order-v1",
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

    safeSetItem("aoe-sidebar-sunk-expanded", "1");
    expect(handler).toHaveBeenCalledWith("aoe-sidebar-sunk-expanded", "1");

    handler.mockClear();
    safeSetItem("aoe-sidebar-width", "300"); // device-local, not synced
    expect(handler).not.toHaveBeenCalled();

    safeRemoveItem("aoe-acp-last-tool");
    expect(handler).toHaveBeenCalledWith("aoe-acp-last-tool", null);
  });
});

describe("hydrateWebUiStateFromServer", () => {
  it("backfills local keys only when the server blob is empty (first-ever sync)", async () => {
    localStorage.setItem("aoe-acp-last-tool", "claude");
    getWebUiState.mockResolvedValue({});

    await hydrateWebUiStateFromServer();

    expect(patchWebUiState).toHaveBeenCalledTimes(1);
    expect(patchWebUiState).toHaveBeenCalledWith({ "aoe-acp-last-tool": "claude" });
  });

  it("server values win and local-only keys are not resurrected once the server is non-empty", async () => {
    localStorage.setItem("aoe-sidebar-sunk-expanded", "0");
    localStorage.setItem("aoe-acp-last-tool", "claude");
    getWebUiState.mockResolvedValue({ "aoe-sidebar-sunk-expanded": "1" });

    await hydrateWebUiStateFromServer();

    expect(localStorage.getItem("aoe-sidebar-sunk-expanded")).toBe("1");
    expect(patchWebUiState).not.toHaveBeenCalled();
  });

  it("leaves the local cache untouched when the fetch fails", async () => {
    localStorage.setItem("aoe-sidebar-sunk-expanded", "0");
    getWebUiState.mockResolvedValue(null);

    await hydrateWebUiStateFromServer();

    expect(localStorage.getItem("aoe-sidebar-sunk-expanded")).toBe("0");
    expect(patchWebUiState).not.toHaveBeenCalled();
  });
});

// Last in the file: `initWebUiSync` is one-shot per module, so this is the only case that arms it.
describe("collapse sync write", () => {
  it("sends a collapse write to the server after the debounce", () => {
    vi.useFakeTimers();
    try {
      initWebUiSync();
      safeSetItem("aoe-group-collapsed-feature", "1");
      expect(patchWebUiState).not.toHaveBeenCalled();
      safeSetItem("aoe-group-collapsed-fix", "1");
      expect(patchWebUiState).not.toHaveBeenCalled();
      vi.advanceTimersByTime(400);
      expect(patchWebUiState).toHaveBeenCalledTimes(1);
      expect(patchWebUiState).toHaveBeenCalledWith({
        "aoe-group-collapsed-feature": "1",
        "aoe-group-collapsed-fix": "1",
      });
    } finally {
      vi.useRealTimers();
    }
  });
});
