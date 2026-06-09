// @vitest-environment jsdom
//
// Contract test for the command-palette action list (#1643). Asserts the
// "New scratch session" command is present with the right shape and dispatches
// onNewScratch, and that both creation commands are hidden in read-only mode
// (matching the sidebar / dashboard, which hide their "new" buttons rather than
// offering a command that opens a wizard the server 403s on submit).

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useCommandActions } from "../useCommandActions";
import type { SessionResponse } from "../../lib/types";

type Args = Parameters<typeof useCommandActions>[0];

function baseArgs(overrides: Partial<Args> = {}): Args {
  return {
    sessions: [] as SessionResponse[],
    activeSessionId: null,
    loginRequired: false,
    hasActiveSession: false,
    readOnly: false,
    onNewSession: vi.fn(),
    onNewScratch: vi.fn(),
    onSelectSession: vi.fn(),
    onToggleDiff: vi.fn(),
    onOpenSettings: vi.fn(),
    onOpenHelp: vi.fn(),
    onOpenAbout: vi.fn(),
    onGoDashboard: vi.fn(),
    onToggleSidebar: vi.fn(),
    onLogout: vi.fn(),
    ...overrides,
  };
}

describe("useCommandActions: scratch command", () => {
  it("exposes a 'New scratch session' command", () => {
    const { result } = renderHook(() => useCommandActions(baseArgs()));
    const scratch = result.current.find((a) => a.id === "action:new-scratch-session");
    expect(scratch).toBeDefined();
    expect(scratch?.title).toBe("New scratch session");
    expect(scratch?.group).toBe("Actions");
    expect(scratch?.keywords).toContain("scratch");
    expect(scratch?.shortcut).toMatch(/N$/);
  });

  it("renders the scratch command right after 'New session'", () => {
    const { result } = renderHook(() => useCommandActions(baseArgs()));
    const ids = result.current.map((a) => a.id);
    const newSession = ids.indexOf("action:new-session");
    const scratch = ids.indexOf("action:new-scratch-session");
    expect(newSession).toBeGreaterThanOrEqual(0);
    expect(scratch).toBe(newSession + 1);
  });

  it("perform dispatches onNewScratch", () => {
    const onNewScratch = vi.fn();
    const { result } = renderHook(() => useCommandActions(baseArgs({ onNewScratch })));
    const scratch = result.current.find((a) => a.id === "action:new-scratch-session");
    scratch?.perform();
    expect(onNewScratch).toHaveBeenCalledTimes(1);
  });

  it("hides both creation commands in read-only mode", () => {
    const { result } = renderHook(() => useCommandActions(baseArgs({ readOnly: true })));
    const ids = result.current.map((a) => a.id);
    expect(ids).not.toContain("action:new-session");
    expect(ids).not.toContain("action:new-scratch-session");
  });
});
