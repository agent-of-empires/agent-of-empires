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
  it("shows the pending placeholder with Host preselected, or a prompt with no session", () => {
    // Never-resolving promise pins ensureState at "pending".
    ensureTerminal.mockReturnValue(new Promise(() => {}));
    render(<PairedShellPane session={makeSession()} sessionId="sess-rp-1" />);
    expect(screen.getByText(/Starting session/i)).toBeDefined();
    expect(screen.getAllByRole("button", { name: /^Host$/ }).length).toBeGreaterThan(0);
    cleanup();
    render(<PairedShellPane session={null} sessionId={null} />);
    expect(screen.getByText(/Select a session/i)).toBeDefined();
  });
});
