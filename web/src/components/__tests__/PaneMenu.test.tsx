// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { PaneMenu } from "../PaneMenu";
import { BUILTIN_PANES } from "../../lib/panes";

const descriptorFor = (id: string) => {
  const d = BUILTIN_PANES.find((p) => p.id === id)!;
  return { title: d.title, icon: d.icon };
};

describe("PaneMenu", () => {
  it("checks open panes, toggles the picked one and closes, and hands focus back on Escape", () => {
    const onToggle = vi.fn();
    render(
      <PaneMenu
        paneIds={["diff", "files"]}
        descriptorFor={descriptorFor}
        openIds={["diff"]}
        onToggle={onToggle}
        className=""
      />,
    );
    const trigger = screen.getByRole("button", { name: "Panes" });
    fireEvent.click(trigger);
    const items = screen.getAllByRole("menuitemcheckbox");
    expect(items.map((i) => [i.textContent, i.getAttribute("aria-checked")])).toEqual([
      ["Diff", "true"],
      ["Files", "false"],
    ]);
    expect(document.activeElement).toBe(items[0]);

    fireEvent.click(screen.getByTestId("pane-toggle-files"));
    expect(onToggle).toHaveBeenCalledWith("files");
    expect(screen.queryByRole("menu")).toBeNull();

    fireEvent.click(trigger);
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect([screen.queryByRole("menu"), document.activeElement]).toEqual([null, trigger]);
  });
});
