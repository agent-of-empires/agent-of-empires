// @vitest-environment jsdom
//
// Contract test for useKeyboardShortcuts. We care most about a single
// regression risk introduced when wterm was swapped for xterm.js:
// xterm's helper textarea calls stopPropagation on a handful of
// modifier-key combos, so the hook must attach the document listener
// in capture phase to observe the keydown before xterm.js swallows it.
//
// Live Playwright tests in terminal-focus-shortcut.spec.ts cover the
// full flow against a mounted terminal; this is the cheap unit-level
// guard that catches "someone moved this back to bubble phase" before
// the e2e suite ever runs.

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useKeyboardShortcuts } from "./useKeyboardShortcuts";

function dispatch(target: EventTarget, init: KeyboardEventInit) {
  const event = new KeyboardEvent("keydown", {
    bubbles: true,
    cancelable: true,
    ...init,
  });
  target.dispatchEvent(event);
  return event;
}

function makeActions() {
  return {
    onNew: vi.fn(),
    onDiff: vi.fn(),
    onEscape: vi.fn(),
    onHelp: vi.fn(),
    onSettings: vi.fn(),
    onPalette: vi.fn(),
    onToggleSidebar: vi.fn(),
    onToggleRightPanel: vi.fn(),
    onToggleTerminalFocus: vi.fn(),
    onPreviousProject: vi.fn(),
    onNextProject: vi.fn(),
  };
}

describe("useKeyboardShortcuts", () => {
  it("fires onPalette for Ctrl+K dispatched on a nested target", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    dispatch(document.body, { key: "k", ctrlKey: true });

    expect(actions.onPalette).toHaveBeenCalledTimes(1);
  });

  it("still fires when a child element calls stopPropagation in bubble phase", () => {
    // Mirror what xterm.js's helper textarea does to Cmd/Ctrl + letter
    // combos: handle them on its own element and stopPropagation so the
    // event would normally never reach document. The capture-phase
    // attachment installed by the hook means we see the keydown before
    // the child's bubble-phase listener runs.
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    const child = document.createElement("textarea");
    document.body.appendChild(child);
    child.addEventListener("keydown", (e) => e.stopPropagation());

    dispatch(child, { key: "k", ctrlKey: true });

    expect(actions.onPalette).toHaveBeenCalledTimes(1);
    child.remove();
  });

  it("routes Ctrl+Alt+B (KeyB) to onToggleRightPanel", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    dispatch(document.body, {
      key: "b",
      code: "KeyB",
      ctrlKey: true,
      altKey: true,
    });

    expect(actions.onToggleRightPanel).toHaveBeenCalledTimes(1);
    expect(actions.onToggleSidebar).not.toHaveBeenCalled();
  });

  it("routes Ctrl+Alt+H and Ctrl+Alt+L to project navigation actions by default", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    dispatch(document.body, {
      key: "h",
      code: "KeyH",
      ctrlKey: true,
      altKey: true,
    });
    dispatch(document.body, {
      key: "l",
      code: "KeyL",
      ctrlKey: true,
      altKey: true,
    });

    expect(actions.onPreviousProject).toHaveBeenCalledTimes(1);
    expect(actions.onNextProject).toHaveBeenCalledTimes(1);
  });

  it("routes Ctrl+H and Ctrl+L when configured", () => {
    const actions = { ...makeActions(), projectStripShortcut: "ctrl-hl" as const };
    renderHook(() => useKeyboardShortcuts(() => actions));

    dispatch(document.body, { key: "h", code: "KeyH", ctrlKey: true });
    dispatch(document.body, { key: "l", code: "KeyL", ctrlKey: true });

    expect(actions.onPreviousProject).toHaveBeenCalledTimes(1);
    expect(actions.onNextProject).toHaveBeenCalledTimes(1);
  });

  it("does not steal project navigation shortcuts from regular text inputs", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));
    const input = document.createElement("input");
    document.body.appendChild(input);

    dispatch(input, { key: "h", code: "KeyH", ctrlKey: true, altKey: true });

    expect(actions.onPreviousProject).not.toHaveBeenCalled();
    input.remove();
  });

  it("allows project navigation from xterm helper textarea focus", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));
    const textarea = document.createElement("textarea");
    textarea.className = "xterm-helper-textarea";
    document.body.appendChild(textarea);

    dispatch(textarea, { key: "l", code: "KeyL", ctrlKey: true, altKey: true });

    expect(actions.onNextProject).toHaveBeenCalledTimes(1);
    textarea.remove();
  });

  it("detaches the listener on unmount", () => {
    const actions = makeActions();
    const { unmount } = renderHook(() => useKeyboardShortcuts(() => actions));

    unmount();
    dispatch(document.body, { key: "k", ctrlKey: true });

    expect(actions.onPalette).not.toHaveBeenCalled();
  });
});
