// @vitest-environment jsdom
//
// BottomDock wraps Dock in a height-resizable strip. Verify it mounts the
// active tab and exposes the height-resize handle (the bit Dock.test does not
// cover).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { BottomDock } from "../BottomDock";
import { BUILTIN_PANES } from "../../lib/panes";

afterEach(() => cleanup());

const body = (id: string) => <div data-testid={`body-${id}`}>{id}</div>;
const descriptorFor = (id: string) => {
  const kind = id.startsWith("terminal") ? "terminal" : id;
  const d = BUILTIN_PANES.find((p) => p.id === kind)!;
  return { title: d.title, icon: d.icon };
};

function renderBottomDock(props = {}) {
  return render(
    <BottomDock
      groups={[{ group: 0, tabs: ["terminal:0"], active: "terminal:0" }]}
      descriptorFor={descriptorFor}
      renderBody={body}
      onActivate={vi.fn()}
      onClose={vi.fn()}
      onMove={vi.fn()}
      onNewTerminal={vi.fn()}
      {...props}
    />,
  );
}

describe("BottomDock", () => {
  it("mounts the active tab and the height-resize handle", () => {
    const { getByText, getByTestId } = renderBottomDock();
    expect(getByText("Terminal")).toBeTruthy();
    expect(getByTestId("body-terminal:0")).toBeTruthy();
    expect(getByTestId("bottom-dock-resize")).toBeTruthy();
  });

  it("forwards the close control to its callback", () => {
    const onClose = vi.fn();
    const { getByLabelText } = renderBottomDock({ onClose });
    fireEvent.click(getByLabelText("Close terminal"));
    expect(onClose).toHaveBeenCalledWith("terminal:0");
  });
});
