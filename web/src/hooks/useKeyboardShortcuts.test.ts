// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useKeyboardShortcuts, type ShortcutActions } from "./useKeyboardShortcuts";

function dispatch(target: EventTarget, init: KeyboardEventInit) {
  const event = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event);
  return event;
}

function mount() {
  const actions = {
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
  } satisfies ShortcutActions;
  return { actions, ...renderHook(() => useKeyboardShortcuts(() => actions)) };
}

type ActionName = keyof ShortcutActions;

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

  it.each([
    { selector: '[data-term="agent"]', label: "agent terminal" },
    { selector: '[data-term="paired"]', label: "paired terminal" },
    { selector: "[data-session-composer]", label: "structured composer" },
  ])("returns focus to the sidebar from the $label", ({ selector }) => {
    const { actions, unmount } = mount();
    const container = document.createElement("div");
    if (selector === '[data-term="agent"]') container.dataset.term = "agent";
    else if (selector === '[data-term="paired"]') container.dataset.term = "paired";
    else container.dataset.sessionComposer = "";
    const input = document.createElement("textarea");
    container.appendChild(input);
    document.body.appendChild(container);

    const event = dispatch(input, { key: "q", code: "KeyQ", ctrlKey: true });

    expect(event.defaultPrevented).toBe(true);
    expect(actions.onFocusSidebar).toHaveBeenCalledOnce();
    expect(actions.onJumpToAttention).not.toHaveBeenCalled();
    unmount();
    container.remove();
  });

  it("leaves Ctrl+Q in a regular input and Cmd+Q in a session input to the browser", () => {
    const { actions, unmount } = mount();
    const regularInput = document.createElement("input");
    document.body.appendChild(regularInput);
    const regularEvent = dispatch(regularInput, { key: "q", code: "KeyQ", ctrlKey: true });

    const terminal = document.createElement("div");
    terminal.dataset.term = "agent";
    const terminalInput = document.createElement("textarea");
    terminal.appendChild(terminalInput);
    document.body.appendChild(terminal);
    const commandEvent = dispatch(terminalInput, { key: "q", code: "KeyQ", metaKey: true });

    expect(regularEvent.defaultPrevented).toBe(false);
    expect(commandEvent.defaultPrevented).toBe(false);
    expect(actions.onFocusSidebar).not.toHaveBeenCalled();
    unmount();
    regularInput.remove();
    terminal.remove();
  });

  it("keeps bare shortcuts textless", () => {
    const { actions, unmount } = mount();
    dispatch(document.body, { key: "n" });
    expect(actions.onNew).toHaveBeenCalledOnce();

    const input = document.createElement("input");
    document.body.appendChild(input);
    dispatch(input, { key: "n" });
    expect(actions.onNew).toHaveBeenCalledOnce();
    unmount();
    input.remove();
  });
});
