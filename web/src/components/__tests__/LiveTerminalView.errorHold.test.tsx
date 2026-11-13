// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LiveTerminalView } from "../LiveTerminalView";
import { installResizeObserver } from "./liveTerminalHarness";
import { makeSession } from "./fixtures";
import type { SessionResponse } from "../../lib/types";

const { ensureSession, restartSession } = vi.hoisted(() => ({
  ensureSession: vi.fn(async () => ({ ok: true, message: null })),
  restartSession: vi.fn<(id: string) => Promise<{ ok: boolean; message?: string }>>(),
}));

vi.mock("../../lib/token", () => ({ getToken: () => null }));
vi.mock("../../lib/deviceBinding", () => ({ getOrCreateDeviceBindingSecret: () => "test-secret" }));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => false }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", () => ({
  ensureSession,
  ensureTerminal: vi.fn(async () => ({ ok: true })),
  restartSession,
  pasteImage: vi.fn(),
  isStartRefusal: () => false,
  reportTelemetrySeen: vi.fn(),
}));
const resize = installResizeObserver();

const sockets: string[] = [];
class FakeWS {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  readyState = 0;
  binaryType = "blob";
  onopen = null;
  onmessage = null;
  onclose = null;
  constructor(url: string) {
    sockets.push(url);
  }
  send() {}
  close() {}
}

beforeEach(() => {
  sockets.length = 0;
  ensureSession.mockClear();
  restartSession.mockReset();
  vi.stubGlobal("WebSocket", FakeWS);
});
afterEach(() => {
  cleanup();
  resize.clear();
  vi.unstubAllGlobals();
});

const errored = (over: Partial<SessionResponse> = {}) =>
  makeSession({ id: "e1", title: "Groceries", status: "Error", last_error: "resume failed\nsecond line", ...over });

describe("LiveTerminalView holds an Error Session instead of starting it (#5055)", () => {
  it.each<[string, SessionResponse, boolean, boolean]>([
    ["an Error Session shows the panel and isn't ensured", errored(), false, true],
    ["a read-only Error Session shows the panel and isn't observed", errored(), true, true],
    ["an Idle Session is ensured as before", makeSession({ id: "i1" }), false, false],
  ])("%s", async (_name, session, readOnly, held) => {
    render(<LiveTerminalView readOnly={readOnly} session={session} />);
    if (held) {
      expect(screen.getByTestId("session-error-panel")).toBeTruthy();
      expect(screen.queryByTestId("session-error-restart") === null).toBe(readOnly);
      await act(async () => {});
      expect(ensureSession).not.toHaveBeenCalled();
      expect(sockets).toEqual([]);
    } else {
      await waitFor(() => expect(ensureSession).toHaveBeenCalledTimes(1));
      expect(screen.queryByTestId("session-error-panel")).toBeNull();
    }
  });

  it("starts the terminal only once Restart takes the Session out of Error", async () => {
    restartSession.mockResolvedValue({ ok: true });
    const { rerender } = render(<LiveTerminalView readOnly={false} session={errored()} />);
    fireEvent.click(screen.getByTestId("session-error-restart"));
    await waitFor(() => expect(restartSession).toHaveBeenCalledWith("e1"));
    expect(ensureSession).not.toHaveBeenCalled();

    rerender(<LiveTerminalView readOnly={false} session={errored({ status: "Starting", last_error: null })} />);
    await waitFor(() => expect(ensureSession).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(sockets).toHaveLength(1));
    expect(screen.queryByTestId("session-error-panel")).toBeNull();
  });

  it("keeps the panel without Restart while the held Session is deleted", async () => {
    const { rerender } = render(<LiveTerminalView readOnly={false} session={errored()} />);
    rerender(<LiveTerminalView readOnly={false} session={errored({ status: "Deleting" })} />);
    expect(screen.getByTestId("session-error-panel")).toBeTruthy();
    expect(screen.queryByTestId("session-error-restart")).toBeNull();
    await act(async () => {});
    expect(ensureSession).not.toHaveBeenCalled();
  });
});
