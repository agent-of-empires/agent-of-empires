// @vitest-environment jsdom

import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { toastBus } from "../../lib/toastBus";
import { useSessionLifecycle } from "./useSessionLifecycle";

const api = vi.hoisted(() => ({
  acpDisable: vi.fn(),
  acpEnable: vi.fn(),
  startSession: vi.fn(),
  stopSession: vi.fn(),
}));

vi.mock("../../lib/api", () => api);

function renderLifecycle() {
  return renderHook(() =>
    useSessionLifecycle({
      workspaces: [],
      trashedWorkspaces: [],
      activeSessionId: null,
      setSessionStatus: vi.fn(),
      applySession: vi.fn(),
      navigate: vi.fn(),
    }),
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  toastBus.handler = {
    push: vi.fn(),
    error: vi.fn(),
    info: vi.fn(),
    openLink: vi.fn(),
  };
});

afterEach(() => {
  cleanup();
  toastBus.handler = null;
});

describe("useSessionLifecycle view switching", () => {
  it("surfaces the daemon reason when switching to terminal fails", async () => {
    api.acpDisable.mockResolvedValue({ ok: false, message: "Resume context could not be preserved" });
    const { result } = renderLifecycle();

    act(() => result.current.requestSwitchView("session-1", false));
    await act(async () => result.current.confirmSwitchView());

    expect(api.acpDisable).toHaveBeenCalledWith("session-1");
    expect(toastBus.handler?.error).toHaveBeenCalledWith("Resume context could not be preserved");
    expect(toastBus.handler?.info).not.toHaveBeenCalled();
    expect(result.current.switchViewTarget).toBeNull();
  });

  it("reports a successful switch to structured view", async () => {
    api.acpEnable.mockResolvedValue(true);
    const { result } = renderLifecycle();

    act(() => result.current.requestSwitchView("session-2", true));
    await act(async () => result.current.confirmSwitchView());

    expect(api.acpEnable).toHaveBeenCalledWith("session-2");
    expect(toastBus.handler?.info).toHaveBeenCalledWith("Switched to structured view");
    expect(toastBus.handler?.error).not.toHaveBeenCalled();
    expect(result.current.switchViewTarget).toBeNull();
  });
});
