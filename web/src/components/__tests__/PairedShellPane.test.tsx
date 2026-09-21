// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { makeSession as baseSession } from "./fixtures";

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

const makeSession = () =>
  baseSession({ id: "sess-rp-1", title: "rp-test", project_path: "/tmp/test", status: "Running" });

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
