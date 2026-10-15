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
});
