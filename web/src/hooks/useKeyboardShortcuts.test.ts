// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useKeyboardShortcuts } from "./useKeyboardShortcuts";

function dispatch(target: EventTarget, init: KeyboardEventInit) {
  target.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init }));
}

function mount() {
  const actions = {
    onNew: vi.fn(),
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
  return { actions, ...renderHook(() => useKeyboardShortcuts(() => actions)) };
}

type ActionName = keyof ReturnType<typeof mount>["actions"];

describe("useKeyboardShortcuts", () => {
  it.each<[string, KeyboardEventInit, ActionName | null, ActionName | null]>([
    [
      "Ctrl+Alt+B toggles the right panel, not the sidebar",
      { key: "b", code: "KeyB", ctrlKey: true, altKey: true },
      "onToggleRightPanel",
      "onToggleSidebar",
    ],
    [
      "Cmd/Ctrl+Shift+N creates a scratch session",
      { key: "N", code: "KeyN", ctrlKey: true, shiftKey: true },
      "onNewScratch",
      "onNew",
    ],
  ])("%s", (_label, init, fired, notFired) => {
    const { actions } = mount();
    dispatch(document.body, init);
    if (fired) expect(actions[fired]).toHaveBeenCalledTimes(1);
    if (notFired) expect(actions[notFired]).not.toHaveBeenCalled();
  });

  it("still fires under a child that stops propagation, and detaches on unmount", () => {
    const { actions, unmount } = mount();
    const child = document.createElement("textarea");
    document.body.appendChild(child);
    child.addEventListener("keydown", (e) => e.stopPropagation());

    dispatch(child, { key: "k", ctrlKey: true });
    expect(actions.onPalette).toHaveBeenCalledTimes(1);

    unmount();
    dispatch(child, { key: "k", ctrlKey: true });
    expect(actions.onPalette).toHaveBeenCalledTimes(1);
    child.remove();
  });
});
