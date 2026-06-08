// @vitest-environment jsdom
//
// Covers PairedShellPane / PairedTerminal render branches: the loading
// placeholder, the connected/reconnecting/disconnected banners, mobile
// chrome, the host/container shell switch, the fullViewport keyboard
// padding, and the focus/keyboard interaction handlers. The live PTY path
// is exercised by the Playwright suites; this drives the conditional JSX
// and the focus latch deterministically with a mocked useTerminal.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import {
  FOCUS_TERMINAL_EVENT,
  setPendingTerminalFocus,
} from "../../lib/terminalFocus";

const ensureTerminal = vi.fn();
const manualReconnect = vi.fn();

const mockState = vi.hoisted(() => ({
  current: {
    connected: true,
    reconnecting: false,
    retryCount: 0,
    isInScrollback: false,
  },
}));
const mockKeyboard = vi.hoisted(() => ({
  current: {
    isMobile: false,
    keyboardOpen: false,
    keyboardHeight: 0,
    keyboardOcclusion: 0,
  },
}));
// A real xterm-like element holding a textarea so focusSelf / toggleKeyboard
// run their actual focus/blur paths rather than the null guard.
const termEl = vi.hoisted(() => ({ current: null as HTMLElement | null }));

vi.mock("../../lib/api", () => ({
  ensureSession: vi.fn(),
  ensureTerminal: (id: string, container: boolean) =>
    ensureTerminal(id, container),
}));

vi.mock("../../hooks/useTerminal", () => ({
  useTerminal: () => ({
    containerRef: { current: null },
    termRef: { current: termEl.current ? { element: termEl.current } : null },
    state: mockState.current,
    manualReconnect,
    sendData: vi.fn(),
    activate: vi.fn(),
    exitScrollback: vi.fn(),
    ctrlActiveRef: { current: false },
    clearCtrlRef: { current: null },
    maxRetries: 7,
  }),
}));

vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => mockKeyboard.current,
}));

vi.mock("../MobileTerminalToolbar", () => ({
  MobileTerminalToolbar: () => <div data-testid="mobile-toolbar" />,
}));
vi.mock("../BackToLiveButton", () => ({
  BackToLiveButton: () => <button data-testid="back-to-live" />,
}));

import { PairedShellPane } from "../PairedTerminal";

function session(overrides: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "sess-1",
    title: "t",
    project_path: "/tmp/t",
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
    ...overrides,
  } as SessionResponse;
}

function makeTermElement() {
  const el = document.createElement("div");
  const ta = document.createElement("textarea");
  el.appendChild(ta);
  return el;
}

beforeEach(() => {
  ensureTerminal.mockResolvedValue(true);
  termEl.current = makeTermElement();
  mockState.current = {
    connected: true,
    reconnecting: false,
    retryCount: 0,
    isInScrollback: false,
  };
  mockKeyboard.current = {
    isMobile: false,
    keyboardOpen: false,
    keyboardHeight: 0,
    keyboardOcclusion: 0,
  };
});

afterEach(() => {
  vi.clearAllMocks();
});

async function renderReady(
  props: Partial<Parameters<typeof PairedShellPane>[0]> = {},
) {
  render(<PairedShellPane session={session()} sessionId="sess-1" {...props} />);
  await waitFor(() =>
    expect(document.querySelector('[data-term="paired"]')).not.toBeNull(),
  );
}

describe("PairedShellPane", () => {
  it("shows the placeholder while ensureTerminal is pending", () => {
    ensureTerminal.mockReturnValue(new Promise(() => {}));
    render(<PairedShellPane session={session()} sessionId="sess-1" />);
    expect(screen.getByText(/Starting terminal/i)).toBeDefined();
  });

  it("renders 'Select a session' when sessionId is null", () => {
    render(<PairedShellPane session={null} sessionId={null} />);
    expect(screen.getByText(/Select a session/i)).toBeDefined();
  });

  it("renders the terminal surface once ready", async () => {
    await renderReady();
    expect(document.querySelector('[data-term="paired"]')).not.toBeNull();
  });

  it("shows an error with Retry when bootstrap fails, then recovers", async () => {
    ensureTerminal.mockResolvedValueOnce(false);
    render(<PairedShellPane session={session()} sessionId="sess-1" />);
    await screen.findByText(/Couldn't start the terminal/i);
    // Retry re-runs ensureTerminal (now succeeding) and the terminal mounts.
    ensureTerminal.mockResolvedValue(true);
    fireEvent.click(screen.getByRole("button", { name: /Retry/i }));
    await waitFor(() =>
      expect(document.querySelector('[data-term="paired"]')).not.toBeNull(),
    );
  });

  it("shows the error state when ensureTerminal rejects", async () => {
    ensureTerminal.mockRejectedValueOnce(new Error("boom"));
    render(<PairedShellPane session={session()} sessionId="sess-1" />);
    await screen.findByText(/Couldn't start the terminal/i);
  });

  it("renders mobile chrome and scrollback affordance", async () => {
    mockKeyboard.current.isMobile = true;
    mockState.current.isInScrollback = true;
    await renderReady();
    expect(screen.getByTestId("mobile-toolbar")).toBeDefined();
    expect(screen.getByTestId("back-to-live")).toBeDefined();
  });

  it("focuses the textarea when the FAB opens the keyboard", async () => {
    mockKeyboard.current.isMobile = true;
    await renderReady();
    const ta = termEl.current!.querySelector("textarea") as HTMLTextAreaElement;
    const focusSpy = vi.spyOn(ta, "focus");
    fireEvent.click(screen.getByRole("button", { name: /Open keyboard/i }));
    expect(focusSpy).toHaveBeenCalled();
  });

  it("blurs the textarea when the FAB closes the keyboard", async () => {
    mockKeyboard.current.isMobile = true;
    mockKeyboard.current.keyboardOpen = true;
    await renderReady();
    const ta = termEl.current!.querySelector("textarea") as HTMLTextAreaElement;
    const blurSpy = vi.spyOn(ta, "blur");
    fireEvent.click(screen.getByRole("button", { name: /Close keyboard/i }));
    expect(blurSpy).toHaveBeenCalled();
  });

  it("ignores focus events aimed at the agent terminal", async () => {
    await renderReady();
    const ta = termEl.current!.querySelector("textarea") as HTMLTextAreaElement;
    const focusSpy = vi.spyOn(ta, "focus");
    act(() => {
      window.dispatchEvent(
        new CustomEvent(FOCUS_TERMINAL_EVENT, { detail: { target: "agent" } }),
      );
    });
    expect(focusSpy).not.toHaveBeenCalled();
  });

  it("no-ops the keyboard toggle when the terminal element is absent", async () => {
    mockKeyboard.current.isMobile = true;
    termEl.current = null;
    await renderReady();
    // FAB present but termRef has no element; toggling must not throw.
    fireEvent.click(screen.getByRole("button", { name: /Open keyboard/i }));
    expect(document.querySelector('[data-term="paired"]')).not.toBeNull();
  });

  it("latches focus when the textarea is not in the DOM yet", async () => {
    // Element present but without a textarea: focusSelf returns false and
    // the paired focus intent is latched instead.
    termEl.current = document.createElement("div");
    await renderReady();
    act(() => {
      window.dispatchEvent(
        new CustomEvent(FOCUS_TERMINAL_EVENT, { detail: { target: "paired" } }),
      );
    });
    expect(document.querySelector('[data-term="paired"]')).not.toBeNull();
  });

  it("focuses itself on a paired focus event", async () => {
    await renderReady();
    const ta = termEl.current!.querySelector("textarea") as HTMLTextAreaElement;
    const focusSpy = vi.spyOn(ta, "focus");
    act(() => {
      window.dispatchEvent(
        new CustomEvent(FOCUS_TERMINAL_EVENT, { detail: { target: "paired" } }),
      );
    });
    expect(focusSpy).toHaveBeenCalled();
  });

  it("consumes a pending paired focus latch once ready", async () => {
    setPendingTerminalFocus("paired");
    const focusedBefore = makeTermElement();
    termEl.current = focusedBefore;
    const ta = focusedBefore.querySelector("textarea") as HTMLTextAreaElement;
    const focusSpy = vi.spyOn(ta, "focus");
    await renderReady();
    await waitFor(() => expect(focusSpy).toHaveBeenCalled());
  });

  it("tracks focus on the terminal surface", async () => {
    await renderReady();
    const panel = document.querySelector('[data-term="paired"]') as HTMLElement;
    fireEvent.focus(panel);
    expect(panel.className).toMatch(/term-focused/);
    fireEvent.blur(panel);
    expect(panel.className).not.toMatch(/term-focused/);
  });

  it("shows the reconnecting banner", async () => {
    mockState.current = {
      connected: false,
      reconnecting: true,
      retryCount: 2,
      isInScrollback: false,
    };
    await renderReady();
    expect(screen.getByText(/Reconnecting/i)).toBeDefined();
  });

  it("shows the disconnected banner with a working Retry", async () => {
    mockState.current = {
      connected: false,
      reconnecting: false,
      retryCount: 7,
      isInScrollback: false,
    };
    await renderReady();
    fireEvent.click(screen.getByRole("button", { name: /Retry/i }));
    expect(manualReconnect).toHaveBeenCalled();
  });

  it("offers the Container switch for sandboxed sessions and re-ensures on switch", async () => {
    render(
      <PairedShellPane
        session={session({ is_sandboxed: true })}
        sessionId="sess-1"
      />,
    );
    await waitFor(() =>
      expect(document.querySelector('[data-term="paired"]')).not.toBeNull(),
    );
    fireEvent.click(screen.getByRole("button", { name: /^Container$/ }));
    await waitFor(() =>
      expect(ensureTerminal).toHaveBeenCalledWith("sess-1", true),
    );
  });

  it("reserves keyboard padding in fullViewport mode", async () => {
    mockKeyboard.current.keyboardOcclusion = 320;
    await renderReady({ fullViewport: true });
    const root = document.querySelector('[data-term="paired"]')
      ?.parentElement as HTMLElement;
    expect(root.style.paddingBottom).toBe("320px");
  });
});
