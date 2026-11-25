// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";

import { ActivityBar, RightDockToggle } from "../ActivityBar";
import { BUILTIN_PANES } from "../../lib/panes";

const descriptorFor = (id: string) => {
  const d = BUILTIN_PANES.find((p) => p.id === id)!;
  return { title: d.title, icon: d.icon };
};

describe("ActivityBar", () => {
  it("renders one toggle per pane reflecting open state and toggles by id", () => {
    const onToggle = vi.fn();
    const { getByTestId } = render(
      <ActivityBar paneIds={["terminal"]} descriptorFor={descriptorFor} isOpen={() => false} onToggle={onToggle} />,
    );
    expect(getByTestId("pane-toggle-terminal").getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(getByTestId("pane-toggle-terminal"));
    expect(onToggle).toHaveBeenCalledWith("terminal");
  });
});

describe("RightDockToggle", () => {
  it("counts changed files only while the review panel is closed", () => {
    const onToggle = vi.fn();
    const closed = render(<RightDockToggle open={false} onToggle={onToggle} badge={3} />);
    const button = closed.getByTestId("right-dock-toggle");
    expect([button.getAttribute("aria-pressed"), button.getAttribute("aria-label")]).toEqual([
      "false",
      "Toggle review panel, 3 changed",
    ]);
    expect(closed.getByTestId("right-dock-badge").textContent).toBe("3");
    fireEvent.click(button);
    expect(onToggle).toHaveBeenCalledOnce();
    closed.unmount();

    const open = render(<RightDockToggle open onToggle={vi.fn()} badge={3} />);
    expect(open.queryByTestId("right-dock-badge")).toBeNull();
    expect(open.getByTestId("right-dock-toggle").getAttribute("aria-label")).toBe("Toggle review panel");
  });
});
