// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { TerminalSwitcherButton, TerminalSwitcherOverlay } from "../TerminalSwitcher";
import type { PairedShellGroup } from "../../lib/pairedShells";
import { makeSession } from "./fixtures";

const groups: PairedShellGroup[] = [
  {
    session: makeSession({ id: "a", title: "Alpha" }),
    shells: [{ sessionId: "a", index: 0, mode: "host", open: true }],
  },
  {
    session: makeSession({ id: "b", title: "Beta", is_sandboxed: true }),
    shells: [
      { sessionId: "b", index: 0, mode: "host", open: false },
      { sessionId: "b", index: 0, mode: "container", open: false },
    ],
  },
];

describe("TerminalSwitcher", () => {
  it("lists shells by Session and picks one, closing the menu", () => {
    const onPick = vi.fn();
    render(<TerminalSwitcherButton groups={groups} newTerminalFor="Alpha" onPick={onPick} onNew={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "Switch terminal" }));
    const menu = screen.getByRole("menu", { name: "Paired terminals" });
    expect(screen.getByRole("group", { name: "Alpha" }).textContent).toContain("open");
    expect(screen.getByTestId("terminal-switcher-item-b-0-container").textContent).toContain("Container");
    // Unsandboxed shells show no mode.
    expect(screen.getByTestId("terminal-switcher-item-a-0-host").textContent).not.toContain("Host");
    expect(menu.textContent).toContain("New terminal for Alpha");
    fireEvent.click(screen.getByTestId("terminal-switcher-item-b-0-container"));
    expect(onPick).toHaveBeenCalledWith(groups[1]!.shells[1]);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("moves through items with the arrow keys from the first, and closes on Escape", () => {
    const onClose = vi.fn();
    const onNew = vi.fn();
    render(
      <TerminalSwitcherOverlay
        groups={groups}
        newTerminalFor="Alpha"
        onPick={vi.fn()}
        onNew={onNew}
        onClose={onClose}
      />,
    );
    const items = screen.getAllByRole("menuitem");
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(document.activeElement!, { key: "ArrowUp" });
    expect(document.activeElement).toBe(screen.getByTestId("terminal-switcher-new"));
    fireEvent.keyDown(document.activeElement!, { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("returns focus to the trigger on Escape, and opens above a trigger near the bottom, capped to the room there", () => {
    render(<TerminalSwitcherButton groups={groups} onPick={vi.fn()} />);
    const trigger = screen.getByRole("button", { name: "Switch terminal" });
    vi.spyOn(trigger, "getBoundingClientRect").mockReturnValue(
      DOMRect.fromRect({ x: 900, y: window.innerHeight - 40, width: 20, height: 20 }),
    );
    fireEvent.click(trigger);
    const menu = screen.getByRole("menu");
    // Portalled out of the dock, whose overflow would clip it.
    expect(menu.parentElement!.parentElement).toBe(document.body);
    expect(menu.style.maxHeight).toBe(`${window.innerHeight - 40 - 4 - 8}px`);
    expect(menu.parentElement!.style.bottom).toBe("44px");
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("the keyboard switcher hands focus back to where it was on Escape", () => {
    const before = document.createElement("button");
    document.body.append(before);
    before.focus();
    const onClose = vi.fn();
    render(<TerminalSwitcherOverlay groups={groups} onPick={vi.fn()} onClose={onClose} />);
    expect(document.activeElement).not.toBe(before);
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect([onClose.mock.calls.length, document.activeElement]).toEqual([1, before]);
    before.remove();
  });

  it("says when no shell is open and offers no new one without a Session", () => {
    render(<TerminalSwitcherOverlay groups={[]} onPick={vi.fn()} onClose={vi.fn()} />);
    expect(screen.getByText("No paired terminals are open")).toBeDefined();
    expect(screen.queryByTestId("terminal-switcher-new")).toBeNull();
  });
});
