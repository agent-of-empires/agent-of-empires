// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, renderHook, screen } from "@testing-library/react";

import { makeSession as baseSession } from "./fixtures";

const ensureTerminal = vi.fn();
vi.mock("../../lib/api", () => ({
  ensureSession: vi.fn(),
  ensureTerminal: (...args: unknown[]) => ensureTerminal(...args),
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
import { usePairedShellModes } from "../../hooks/usePairedShellModes";

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
    render(<PairedShellPane readOnly={false} session={makeSession()} sessionId="sess-rp-1" />);
    expect(screen.getByText(/Starting session/i)).toBeDefined();
    expect(screen.getAllByRole("button", { name: /^Host$/ }).length).toBeGreaterThan(0);
    cleanup();
    render(<PairedShellPane readOnly={false} session={null} sessionId={null} />);
    expect(screen.getByText(/Select a session/i)).toBeDefined();
  });

  it.each([
    [true, true],
    // A Session without a container never opens one, whatever its owner asked for.
    [false, false],
  ])(
    "opens the shell its owner picked: Container, sandboxed %s, opens a container shell: %s",
    (sandboxed, container) => {
      ensureTerminal.mockReturnValue(new Promise(() => {}));
      const onModeChange = vi.fn();
      render(
        <PairedShellPane
          readOnly={false}
          session={{ ...makeSession(), is_sandboxed: sandboxed }}
          sessionId="sess-rp-1"
          mode="container"
          onModeChange={onModeChange}
        />,
      );
      expect(ensureTerminal).toHaveBeenLastCalledWith("sess-rp-1", 0, container);
      fireEvent.click(screen.getByRole("button", { name: /^Host$/ }));
      expect(onModeChange).toHaveBeenCalledWith("host");
    },
  );
});

describe("usePairedShellModes", () => {
  it("keeps each Session's and terminal's mode to itself", () => {
    const { result } = renderHook(() => usePairedShellModes());
    act(() => result.current.setMode("a", 0, "container"));
    expect(result.current.modeFor("a", 0)).toBe("container");
    expect(result.current.modeFor("b", 0)).toBe("host");
    expect(result.current.modeFor("a", 1)).toBe("host");
    expect(result.current.modeFor(null, 0)).toBe("host");
  });
});
