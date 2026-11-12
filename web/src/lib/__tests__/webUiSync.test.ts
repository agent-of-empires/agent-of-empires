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
import { STATUS_COLLAPSE_PREFIX } from "../../hooks/useStatusGroups";

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
      "aoe-sidebar-sort-mode",
      "aoe-sidebar-axis",
      "aoe-sidebar-sunk-expanded",
      "aoe-repo-appearance-v1",
      "aoe-repo-group-order-v1",
      "aoe-acp-last-tool",
      "aoe-last-browse-dir",
      "aoe-web-settings",
      "aoe-suggestions-hint-dismissed",
      "aoe-repo-collapsed-/home/me/repo",
      "aoe-group-collapsed-feature",
      "aoe-nested-group-collapsed-x",
      "aoe-status-collapsed-Waiting",
      "aoe-settings-intro-worktree", // folded Settings tab intro
    ]) {
      expect(isSyncedKey(k), k).toBe(true);
    }
    for (const k of [
      "aoe-local-settings-intro-worktree", // a read-only dashboard's fold stays on the device
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

describe("status collapse keys", () => {
  it("syncs the prefix the status axis writes collapse state under", () => {
    expect(STATUS_COLLAPSE_PREFIX).toBe("aoe-status-collapsed-");
    expect(isSyncedKey(`${STATUS_COLLAPSE_PREFIX}status:Waiting`)).toBe(true);
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
    localStorage.setItem("aoe-sidebar-axis", "repo");
    localStorage.setItem("aoe-acp-last-tool", "claude");
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

// Last in the file: `initWebUiSync` is one-shot per module, so this is the only case that arms it.
describe("status collapse sync write", () => {
  it("sends a status-collapse write to the server after the debounce", () => {
    vi.useFakeTimers();
    try {
      initWebUiSync();
      safeSetItem(`${STATUS_COLLAPSE_PREFIX}status:Waiting`, "1");
      expect(patchWebUiState).not.toHaveBeenCalled();
      safeSetItem(`${STATUS_COLLAPSE_PREFIX}status:Idle`, "1");
      expect(patchWebUiState).not.toHaveBeenCalled();
      vi.advanceTimersByTime(400);
      expect(patchWebUiState).toHaveBeenCalledTimes(1);
      expect(patchWebUiState).toHaveBeenCalledWith({
        "aoe-status-collapsed-status:Waiting": "1",
        "aoe-status-collapsed-status:Idle": "1",
      });
    } finally {
      vi.useRealTimers();
    }
  });
});
