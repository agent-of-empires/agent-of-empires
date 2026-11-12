// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { LiveTerminalView } from "../LiveTerminalView";
import { consumePendingTerminalFocus, dispatchFocusTerminal, setPendingTerminalFocus } from "../../lib/terminalFocus";
import { makeSession } from "./fixtures";

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: () => ({
    state: {
      connected: true,
      reconnecting: false,
      retryCount: 0,
      retryCountdown: 0,
      frame: null,
      reading: false,
      isOwner: true,
      ownerKnown: true,
      transport: null,
      stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
    },
    sendData: vi.fn(),
    sendPaste: vi.fn(),
    maxRetries: 5,
    manualReconnect: vi.fn(),
    claim: vi.fn(),
  }),
}));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => false }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", () => ({
  ensureSession: async () => ({ ok: true, message: null }),
  ensureTerminal: async () => ({ ok: true }),
  isStartRefusal: () => false,
  pasteImage: vi.fn(),
}));
vi.mock("../MobileLiveTerminal", () => ({
  MobileLiveTerminal: ({ inputRef }: { inputRef: RefObject<HTMLTextAreaElement | null> }) => (
    <textarea aria-label="Live terminal input" ref={inputRef} />
  ),
}));

afterEach(() => {
  cleanup();
  setPendingTerminalFocus("agent");
  consumePendingTerminalFocus("agent");
});

/** A paired terminal in each dock, as a right and a bottom group can show at once. */
function renderTwoPaired() {
  const session = makeSession({ id: "two-docks" });
  render(
    <>
      <div data-testid="terminal-1">
        <LiveTerminalView readOnly={false} session={session} surface="paired-host" terminalIndex={1} />
      </div>
      <div data-testid="terminal-2">
        <LiveTerminalView readOnly={false} session={session} surface="paired-host" terminalIndex={2} />
      </div>
    </>,
  );
  return (index: number) => within(screen.getByTestId(`terminal-${index}`)).getByLabelText("Live terminal input");
}

describe("LiveTerminalView paired focus with two docked terminals", () => {
  it.each([1, 2])("a focus request for terminal %i focuses that one", async (index) => {
    const input = renderTwoPaired();
    await act(async () => {});
    act(() => dispatchFocusTerminal("paired", index));
    expect(document.activeElement).toBe(input(index));
  });

  it("a latched focus waits for its own terminal, not the first to be ready", async () => {
    setPendingTerminalFocus("paired", 2);
    const input = renderTwoPaired();
    await waitFor(() => expect(document.activeElement).toBe(input(2)));
  });

  it("a focus request for any paired terminal, sent before either is ready, goes to the first ready", async () => {
    const input = renderTwoPaired();
    act(() => dispatchFocusTerminal("paired"));
    await waitFor(() => expect(document.activeElement).toBe(input(1)));
  });
});
