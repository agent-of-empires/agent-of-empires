// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";

import { ActivityBar } from "../ActivityBar";
import { BUILTIN_PANES } from "../../lib/panes";

const descriptorFor = (id: string) => {
  const d = BUILTIN_PANES.find((p) => p.id === id)!;
  return { title: d.title, icon: d.icon };
};

describe("ActivityBar", () => {
  it("renders one toggle per pane reflecting open state and toggles by id", () => {
    const open = new Set(["diff"]);
    const onToggle = vi.fn();
    const { getByTestId } = render(
      <ActivityBar
        paneIds={["diff", "terminal"]}
        descriptorFor={descriptorFor}
        isOpen={(id) => open.has(id)}
        onToggle={onToggle}
      />,
    );
    expect(getByTestId("pane-toggle-diff").getAttribute("aria-pressed")).toBe("true");
    expect(getByTestId("pane-toggle-terminal").getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(getByTestId("pane-toggle-terminal"));
    expect(onToggle).toHaveBeenCalledWith("terminal");
  });

  it("shows a count only on a closed pane that has one", () => {
    const render_ = (open: boolean) =>
      render(
        <ActivityBar
          paneIds={["diff", "terminal"]}
          descriptorFor={descriptorFor}
          isOpen={() => open}
          onToggle={vi.fn()}
          closedBadge={(id) => (id === "diff" ? 3 : 0)}
        />,
      );
    const closed = render_(false);
    expect(closed.getByTestId("pane-badge-diff").textContent).toBe("3");
    expect(closed.getByTestId("pane-toggle-diff").getAttribute("aria-label")).toBe("Toggle diff pane, 3 changed");
    expect(closed.queryByTestId("pane-badge-terminal")).toBeNull();
    closed.unmount();
    expect(render_(true).queryByTestId("pane-badge-diff")).toBeNull();
  });
});
