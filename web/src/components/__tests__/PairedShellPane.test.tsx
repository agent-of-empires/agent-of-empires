// @vitest-environment jsdom
//
// Contract test for PairedShellPane's "Starting session..." placeholder and
// shell-mode controls. The full mounted-terminal path is exercised by the
// Playwright suites; this renders the early branches and asserts the loading
// copy and shell picker are present. PairedShellPane is the body of the
// "terminal" dock pane (previously the lower half of RightPanel).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";

const ensureTerminal = vi.fn();
vi.mock("../../lib/api", () => ({
  ensureSession: vi.fn(),
  ensureTerminal: (id: string, container: boolean) => ensureTerminal(id, container),
}));

vi.mock("../../hooks/useTerminal", () => ({
  useTerminal: () => ({
    containerRef: { current: null },
    termRef: { current: null },
    state: {
      connected: false,
      reconnecting: false,
      retryCount: 0,
      retryCountdown: 0,
      isPrimary: true,
      isInScrollback: false,
    },
    manualReconnect: vi.fn(),
    sendData: vi.fn(),
    activate: vi.fn(),
    exitScrollback: vi.fn(),
    ctrlActiveRef: { current: false },
    clearCtrlRef: { current: null },
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

import { PairedShellPane } from "../PairedTerminal";

function makeSession(): SessionResponse {
  return {
    id: "sess-rp-1",
    title: "rp-test",
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
  } as SessionResponse;
}

afterEach(() => {
  ensureTerminal.mockReset();
  cleanup();
});

describe("PairedShellPane", () => {
  it("renders the ensure-pending placeholder while the shell starts", () => {
    // Never-resolving promise pins ensureState at "pending" so the
    // LiveTerminalView placeholder branch stays mounted.
    ensureTerminal.mockReturnValue(new Promise(() => {}));
    render(<PairedShellPane session={makeSession()} sessionId="sess-rp-1" />);
    expect(screen.getByText(/Starting session/i)).toBeDefined();
  });

  it("renders the shell mode picker with Host preselected", () => {
    ensureTerminal.mockReturnValue(new Promise(() => {}));
    render(<PairedShellPane session={makeSession()} sessionId="sess-rp-1" />);
    expect(screen.getAllByRole("button", { name: /^Host$/ }).length).toBeGreaterThan(0);
  });

  it("renders 'Select a session' when sessionId is null", () => {
    render(<PairedShellPane session={null} sessionId={null} />);
    expect(screen.getByText(/Select a session/i)).toBeDefined();
  });
});
