// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import { LiveTerminalView } from "../LiveTerminalView";
import { makeSession } from "./fixtures";
import { stubMatchMedia } from "../../hooks/__tests__/fixtures";

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
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => true }));
vi.mock("../../lib/api", () => ({
  ensureSession: async () => ({ ok: true, message: null }),
  ensureTerminal: async () => ({ ok: true }),
  isStartRefusal: () => false,
  pasteImage: vi.fn(),
}));
vi.mock("../MobileLiveTerminal", () => ({
  MobileLiveTerminal: ({
    inputRef,
    onInputFocusChange,
  }: {
    inputRef: RefObject<HTMLTextAreaElement | null>;
    onInputFocusChange: (focused: boolean) => void;
  }) => (
    <textarea
      aria-label="Live terminal input"
      ref={inputRef}
      onFocus={() => onInputFocusChange(true)}
      onBlur={() => onInputFocusChange(false)}
    />
  ),
}));

const FULL_HEIGHT = 800;
const KEYBOARD_UP_HEIGHT = 500;

function stubViewport() {
  const target = new EventTarget();
  const vv = Object.assign(target, { height: FULL_HEIGHT });
  Object.defineProperty(window, "visualViewport", { configurable: true, value: vv });
  return vv;
}

function setVisibility(state: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", { configurable: true, value: state });
  document.dispatchEvent(new Event("visibilitychange"));
}

async function openTerminal() {
  render(<LiveTerminalView readOnly={false} session={makeSession({ id: "ios" })} surface="agent" terminalIndex={0} />);
  await screen.findByRole("button", { name: "Open keyboard" });
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "requestAnimationFrame", "cancelAnimationFrame"] });
  return screen.getByRole<HTMLTextAreaElement>("textbox", { name: "Live terminal input" });
}

function raiseKeyboard(vv: { height: number } & EventTarget, input: HTMLTextAreaElement) {
  act(() => {
    input.focus();
    vv.height = KEYBOARD_UP_HEIGHT;
    vv.dispatchEvent(new Event("resize"));
    vi.advanceTimersByTime(1000);
  });
}

beforeEach(() => {
  stubMatchMedia(true, "(pointer: coarse)");
  Object.defineProperty(window, "innerHeight", { configurable: true, writable: true, value: FULL_HEIGHT });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
  // @ts-expect-error cleanup
  delete window.visualViewport;
  Object.defineProperty(document, "visibilityState", { configurable: true, value: "visible" });
});

describe("LiveTerminalView keyboard FAB across an iOS app switch", () => {
  // activeElement survives the switch. WebKit replays blur and focus around page deactivation, but a suspended page
  // may not see them, so both event histories must end the same way.
  it.each([
    { name: "no focus events", replay: false },
    { name: "a replayed focusout and focusin", replay: true },
  ])("offers Open keyboard, and a tap focuses, after $name when the keyboard stays down", async ({ replay }) => {
    const vv = stubViewport();
    const input = await openTerminal();
    raiseKeyboard(vv, input);
    expect(screen.getByRole("button", { name: "Close keyboard" })).toBeTruthy();

    act(() => {
      if (replay) input.dispatchEvent(new FocusEvent("focusout", { bubbles: true }));
      setVisibility("hidden");
      // No resize: viewport events can be missed while suspended.
      vv.height = FULL_HEIGHT;
      setVisibility("visible");
      if (replay) input.dispatchEvent(new FocusEvent("focusin", { bubbles: true }));
      vi.advanceTimersByTime(1000);
    });

    expect(document.activeElement).not.toBe(input);
    const fab = screen.getByRole("button", { name: "Open keyboard" });
    act(() => fab.click());
    expect(document.activeElement).toBe(input);
  });

  it("keeps focus when iOS restores the keyboard on return", async () => {
    const vv = stubViewport();
    const input = await openTerminal();
    raiseKeyboard(vv, input);

    act(() => {
      setVisibility("hidden");
      setVisibility("visible");
      vi.advanceTimersByTime(1000);
    });

    expect(document.activeElement).toBe(input);
    expect(screen.getByRole("button", { name: "Close keyboard" })).toBeTruthy();
  });

  // A hardware keyboard never occludes the viewport, so absence of occlusion says nothing about it.
  it("keeps focus when no soft keyboard was ever seen for this focus", async () => {
    const vv = stubViewport();
    const input = await openTerminal();
    act(() => input.focus());

    act(() => {
      setVisibility("hidden");
      setVisibility("visible");
      vi.advanceTimersByTime(1000);
    });

    expect(vv.height).toBe(FULL_HEIGHT);
    expect(document.activeElement).toBe(input);
    expect(screen.getByRole("button", { name: "Close keyboard" })).toBeTruthy();
  });
});
