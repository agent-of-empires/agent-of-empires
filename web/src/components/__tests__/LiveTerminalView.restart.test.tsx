// @vitest-environment jsdom

import type { RefObject } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LiveTerminalView } from "../LiveTerminalView";
import { makeSession } from "./fixtures";

const ensureSession = vi.hoisted(() => vi.fn(async () => ({ ok: true, message: null })));

vi.mock("../../lib/token", () => ({ getToken: () => null }));
vi.mock("../../lib/deviceBinding", () => ({ getOrCreateDeviceBindingSecret: () => "test-secret" }));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => true }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", () => ({
  ensureSession,
  ensureTerminal: async () => ({ ok: true, message: null }),
  pasteImage: vi.fn(),
  isStartRefusal: () => false,
  reportTelemetrySeen: vi.fn(),
}));
vi.mock("../MobileLiveTerminal", () => ({
  MobileLiveTerminal: ({ inputRef }: { inputRef: RefObject<HTMLTextAreaElement | null> }) => (
    <textarea aria-label="Live terminal input" ref={inputRef} />
  ),
}));

class FakeWS {
  static sockets: FakeWS[] = [];
  static OPEN = 1;
  static CLOSED = 3;
  readyState = 0;
  binaryType = "blob";
  onopen: ((e: Event) => void) | null = null;
  onmessage: ((e: MessageEvent) => void) | null = null;
  onclose: ((e: CloseEvent) => void) | null = null;
  constructor() {
    FakeWS.sockets.push(this);
  }
  send() {}
  close() {
    this.readyState = FakeWS.CLOSED;
  }
}

beforeEach(() => {
  FakeWS.sockets.length = 0;
  ensureSession.mockClear();
  vi.stubGlobal("WebSocket", FakeWS);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("LiveTerminalView Restart with the real hook", () => {
  it("dials a new socket only after ensure resolves, and clears the exited strip once it opens", async () => {
    render(<LiveTerminalView session={makeSession({ id: "restart" })} />);
    await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
    const dead = FakeWS.sockets[0]!;
    act(() => {
      dead.readyState = FakeWS.CLOSED;
      dead.onclose?.(new CloseEvent("close", { code: 4001 }));
    });

    fireEvent.click(await screen.findByRole("button", { name: "Restart" }));
    await waitFor(() => expect(ensureSession).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(FakeWS.sockets).toHaveLength(2));

    const fresh = FakeWS.sockets[1]!;
    act(() => {
      fresh.readyState = FakeWS.OPEN;
      fresh.onopen?.({} as Event);
    });
    await waitFor(() => expect(screen.queryByText("Agent exited")).toBeNull());
  });
});
