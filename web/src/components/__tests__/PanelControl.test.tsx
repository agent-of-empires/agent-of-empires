// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { PanelControl, TerminalToggle } from "../PanelControl";
import { BUILTIN_PANES } from "../../lib/panes";

const descriptorFor = (id: string) => {
  const d = BUILTIN_PANES.find((p) => p.id === id)!;
  return { title: d.title, icon: d.icon };
};

const setup = (open: boolean) => {
  const onToggle = vi.fn();
  const onTogglePane = vi.fn();
  const view = render(
    <PanelControl
      open={open}
      onToggle={onToggle}
      badge={3}
      paneIds={["diff", "files"]}
      descriptorFor={descriptorFor}
      openIds={["diff"]}
      onTogglePane={onTogglePane}
    />,
  );
  return { ...view, onToggle, onTogglePane };
};

describe("PanelControl", () => {
  it("toggles the review panel, counting changed files only while it is closed", () => {
    const closed = setup(false);
    const button = closed.getByTestId("right-dock-toggle");
    expect([button.getAttribute("aria-pressed"), button.getAttribute("aria-label")]).toEqual([
      "false",
      "Toggle review panel, 3 changed",
    ]);
    expect(closed.getByTestId("right-dock-badge").textContent).toBe("3");
    expect(button.getAttribute("title")).toMatch(/^Show review panel \(.+B\)$/);
    fireEvent.click(button);
    expect(closed.onToggle).toHaveBeenCalledOnce();
    closed.unmount();

    const open = setup(true);
    expect(open.queryByTestId("right-dock-badge")).toBeNull();
  });

  it("lists panes checked while open, moves with the arrow keys, toggles the picked one, and hands focus back on Escape", () => {
    const { onTogglePane } = setup(true);
    const chevron = screen.getByRole("button", { name: "Panes" });
    fireEvent.click(chevron);
    const items = screen.getAllByRole("menuitemcheckbox");
    expect(items.map((i) => [i.textContent, i.getAttribute("aria-checked")])).toEqual([
      ["Diff", "true"],
      ["Files", "false"],
    ]);
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(items[0]!, { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[1]);
    fireEvent.keyDown(items[1]!, { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(items[0]!, { key: "End" });
    expect(document.activeElement).toBe(items[1]);
    fireEvent.click(screen.getByTestId("pane-toggle-files"));
    expect([onTogglePane.mock.calls, screen.queryByRole("menu")]).toEqual([[["files"]], null]);

    fireEvent.click(chevron);
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect([screen.queryByRole("menu"), document.activeElement]).toEqual([null, chevron]);
  });
});

describe("TerminalToggle", () => {
  it("reflects and toggles the terminal", () => {
    const onToggle = vi.fn();
    render(<TerminalToggle open onToggle={onToggle} />);
    const button = screen.getByTestId("pane-toggle-terminal");
    expect(button.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(button);
    expect(onToggle).toHaveBeenCalledOnce();
  });
});
