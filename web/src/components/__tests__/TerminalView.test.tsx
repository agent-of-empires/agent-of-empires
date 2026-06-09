// @vitest-environment jsdom
//
// Contract test for TerminalView's pending / error early-return
// branches. The full mounted-terminal path is exercised by the
// Playwright suites; this test just asserts the loading placeholder
// and the error retry surface render correctly without touching the
// xterm.js mount chain.

import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, act } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";

// ── Mock the chain of dependencies the component pulls in so the
// render stops at the early-return without trying to mount a real
// terminal or open a WebSocket.

const ensureSession = vi.fn(async () => ({ ok: true }));
const mockedContainerRef = { current: null } as const;
const mockedTermRef = { current: null } as const;
const mockedManualReconnect = vi.fn();
const mockedSendData = vi.fn();
const mockedActivate = vi.fn();
const mockedExitScrollback = vi.fn();
const mockedCtrlActiveRef = { current: false };
const mockedClearCtrlRef = { current: null };
vi.mock("../../lib/api", () => ({
  ensureSession: (id: string, signal?: AbortSignal) => ensureSession(id, signal),
  ensureTerminal: vi.fn(),
}));

// The full hook is exercised by useTerminal.lifecycle.test.ts and the
// Playwright suites. Stubbing it here keeps the component test fast
// and free of jsdom canvas warnings.
vi.mock("../../hooks/useTerminal", () => ({
  useTerminal: () => ({
    containerRef: mockedContainerRef,
    termRef: mockedTermRef,
    state: {
      connected: false,
      reconnecting: false,
      retryCount: 0,
      retryCountdown: 0,
      isPrimary: true,
      isInScrollback: false,
    },
    manualReconnect: mockedManualReconnect,
    sendData: mockedSendData,
    activate: mockedActivate,
    exitScrollback: mockedExitScrollback,
    ctrlActiveRef: mockedCtrlActiveRef,
    clearCtrlRef: mockedClearCtrlRef,
    maxRetries: 7,
  }),
}));

vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({
    isMobile: false,
    keyboardOpen: false,
    keyboardHeight: 0,
    keyboardOcclusion: 0,
    stableViewportHeight: 0,
  }),
}));

import { TerminalView } from "../TerminalView";

function makeSession(overrides: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "sess-1",
    title: "test-session",
    project_path: "/tmp/test",
    group_path: "/tmp",
    tool: "claude",
    status: "Running",
    yolo_mode: false,
    created_at: new Date().toISOString(),
    last_accessed_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    has_terminal: true,
    profile: "default",
    workspace_repos: [],
    claude_fullscreen: false,
    ...overrides,
  } as SessionResponse;
}

afterEach(() => {
  ensureSession.mockReset();
  ensureSession.mockImplementation(async () => ({ ok: true }));
  mockedManualReconnect.mockReset();
  mockedSendData.mockReset();
  mockedActivate.mockReset();
  mockedExitScrollback.mockReset();
  mockedCtrlActiveRef.current = false;
  mockedClearCtrlRef.current = null;
});

describe("TerminalView early-return states", () => {
  it("renders the 'Starting session...' placeholder while ensure is pending", () => {
    // Never-resolving promise keeps ensureState at "pending" so the
    // placeholder branch stays mounted.
    ensureSession.mockReturnValue(new Promise(() => {}));
    render(<TerminalView session={makeSession()} />);
    expect(screen.getByText(/Starting session/i)).toBeDefined();
  });

  it("renders the error message + Retry button when ensure rejects", async () => {
    ensureSession.mockResolvedValueOnce({
      ok: false,
      message: "boom",
    });
    render(<TerminalView session={makeSession()} />);
    await waitFor(() => {
      expect(screen.getByText("boom")).toBeDefined();
    });
    const retry = screen.getByRole("button", { name: /retry/i });
    expect(retry).toBeDefined();
  });

  it("falls back to the generic error copy when ensure omits a message", async () => {
    ensureSession.mockResolvedValueOnce({ ok: false });
    render(<TerminalView session={makeSession()} />);
    await waitFor(() => {
      expect(screen.getByText(/Could not start session/i)).toBeDefined();
    });
  });

  it("re-runs ensureSession when Retry is clicked", async () => {
    ensureSession.mockResolvedValueOnce({ ok: false, message: "first fail" });
    const { container } = render(<TerminalView session={makeSession()} />);
    await waitFor(() => {
      expect(screen.getByText("first fail")).toBeDefined();
    });
    ensureSession.mockResolvedValueOnce({ ok: false, message: "second fail" });
    // The error branch only ever renders one button. Scope to it
    // explicitly so this test does not accidentally pick up the
    // reconnect-retry button that the ready branch may also render.
    const retry = container.querySelector("button");
    if (!retry) throw new Error("no retry button rendered");
    await act(async () => {
      retry.click();
    });
    await waitFor(() => {
      expect(screen.getByText("second fail")).toBeDefined();
    });
    // First call (mount), second call (retry).
    expect(ensureSession).toHaveBeenCalledTimes(2);
  });
});
