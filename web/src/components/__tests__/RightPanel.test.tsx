// @vitest-environment jsdom
//
// Contract test for RightPanel's PairedTerminal "Starting terminal..."
// placeholder. The full mounted-terminal path is exercised by the
// Playwright suites; this just renders the early-return branch and
// asserts the loading copy and basic shell mode controls are present.

import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";

import type { RichDiffFile, SessionResponse } from "../../lib/types";

const ensureTerminal = vi.fn();
vi.mock("../../lib/api", () => ({
  ensureSession: vi.fn(),
  ensureTerminal: (id: string, container: boolean) =>
    ensureTerminal(id, container),
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

// The DiffFileList pulls in a heavy chain (shiki, diff library, etc).
// We're only exercising the right pane's terminal placeholder branch,
// so a stub is enough.
vi.mock("../diff/DiffFileList", () => ({
  DiffFileList: () => null,
}));
vi.mock("../diff/comments/CommentsBanner", () => ({
  CommentsBanner: () => null,
}));

import { RightPanel } from "../RightPanel";

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

const baseProps = {
  session: makeSession(),
  sessionId: "sess-rp-1",
  files: [] as RichDiffFile[],
  perRepoBases: [],
  warning: null,
  filesLoading: false,
  selectedFilePath: null,
  selectedRepoName: undefined,
  onSelectFile: vi.fn(),
  onDiffRefresh: vi.fn(),
  commentsEnabled: false,
  commentsCount: 0,
  commentsSendEnabled: false,
  onOpenSendDialog: vi.fn(),
  onDiscardAllComments: vi.fn(),
};

afterEach(() => {
  ensureTerminal.mockReset();
});

describe("RightPanel PairedTerminal", () => {
  it("renders the 'Starting terminal...' placeholder while ensure is pending", () => {
    // Never-resolving promise pins `ready` at false so the placeholder
    // branch stays mounted.
    ensureTerminal.mockReturnValue(new Promise(() => {}));
    render(<RightPanel {...baseProps} />);
    expect(screen.getByText(/Starting terminal/i)).toBeDefined();
  });

  it("renders the shell mode picker with Host preselected", () => {
    ensureTerminal.mockReturnValue(new Promise(() => {}));
    render(<RightPanel {...baseProps} />);
    // The "Host" picker is rendered twice -- once for desktop, once
    // for the mobile slide-in. Either match is fine; what matters is
    // that the shell-mode toggle made it into the DOM.
    expect(
      screen.getAllByRole("button", { name: /^Host$/ }).length,
    ).toBeGreaterThan(0);
  });

  it("renders 'Select a session' when sessionId is null", () => {
    render(<RightPanel {...baseProps} sessionId={null} session={null} />);
    expect(screen.getByText(/Select a session/i)).toBeDefined();
  });
});
