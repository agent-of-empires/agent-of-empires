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
    onFocusSidebar: vi.fn(),
    onJumpToAttention: vi.fn(),
    onNewScratch: vi.fn(),
    onDiff: vi.fn(),
    onEscape: vi.fn(),
    onHelp: vi.fn(),
    onSettings: vi.fn(),
    onPalette: vi.fn(),
    onToggleSidebar: vi.fn(),
    onToggleRightPanel: vi.fn(),
    onToggleTerminalFocus: vi.fn(),
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

  it("routes Cmd/Ctrl+Shift+N to onNewScratch (fast-create shortcut)", () => {
    // Cmd+Shift+N (Mac) / Ctrl+Shift+N (other) is the
    // wizard-pre-configured-for-scratch + skip-to-Review shortcut.
    // Uses `e.code === "KeyN"` so Shift+layout-specific punctuation
    // does not break the match.
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    dispatch(document.body, {
      key: "N",
      code: "KeyN",
      ctrlKey: true,
      shiftKey: true,
    });

    expect(actions.onNewScratch).toHaveBeenCalledTimes(1);
    expect(actions.onNew).not.toHaveBeenCalled();
  });

  it("does NOT fire onNewScratch for plain Shift+N (no modifier)", () => {
    // Single-key shortcuts only fire when no input/textarea is
    // focused AND no modifier is held; "n" with no modifier maps to
    // `onNew`, but the Shift+N path needs both the meta/ctrl
    // modifier AND Shift. Guard against a regression that drops the
    // modifier check.
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    dispatch(document.body, {
      key: "N",
      code: "KeyN",
      shiftKey: true,
    });

    expect(actions.onNewScratch).not.toHaveBeenCalled();
  });

  it("routes Ctrl+Q from the focused embedded terminal to the sidebar", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    const terminal = document.createElement("div");
    terminal.dataset.term = "agent";
    const input = document.createElement("textarea");
    terminal.appendChild(input);
    document.body.appendChild(terminal);
    const event = dispatch(input, { key: "q", code: "KeyQ", ctrlKey: true });

    expect(event.defaultPrevented).toBe(true);
    expect(actions.onFocusSidebar).toHaveBeenCalledOnce();
    expect(actions.onJumpToAttention).not.toHaveBeenCalled();
    terminal.remove();
  });
  it("routes Ctrl+Q from the structured composer to the sidebar", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    const composer = document.createElement("div");
    composer.dataset.sessionComposer = "";
    const input = document.createElement("textarea");
    composer.appendChild(input);
    document.body.appendChild(composer);
    const event = dispatch(input, { key: "q", code: "KeyQ", ctrlKey: true });

    expect(event.defaultPrevented).toBe(true);
    expect(actions.onFocusSidebar).toHaveBeenCalledOnce();
    expect(actions.onJumpToAttention).not.toHaveBeenCalled();
    composer.remove();
  });

  it("does not capture Ctrl+Q from a regular text input", () => {
    const actions = makeActions();
    renderHook(() => useKeyboardShortcuts(() => actions));

    const input = document.createElement("input");
    document.body.appendChild(input);
    const event = dispatch(input, { key: "q", code: "KeyQ", ctrlKey: true });

    expect(event.defaultPrevented).toBe(false);
    expect(actions.onFocusSidebar).not.toHaveBeenCalled();
    input.remove();
  });

  it("detaches the listener on unmount", () => {
    const actions = makeActions();
    const { unmount } = renderHook(() => useKeyboardShortcuts(() => actions));

    unmount();
    dispatch(document.body, { key: "k", ctrlKey: true });

    expect(actions.onPalette).not.toHaveBeenCalled();
  });
});
