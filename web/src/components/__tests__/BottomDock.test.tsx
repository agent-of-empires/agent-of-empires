// @vitest-environment jsdom
//
// BottomDock wraps Dock in a height-resizable strip. Verify it mounts its panes
// and exposes the height-resize handle (the bit Dock.test does not cover).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { BottomDock } from "../BottomDock";
import { BUILTIN_PANES } from "../../lib/panes";

afterEach(() => cleanup());

const body = (id: string) => <div data-testid={`body-${id}`}>{id}</div>;
const descriptorFor = (id: string) => {
  const d = BUILTIN_PANES.find((p) => p.id === id)!;
  return { title: d.title, icon: d.icon };
};

describe("BottomDock", () => {
  it("mounts its panes and the height-resize handle", () => {
    const { getByText, getByTestId } = render(
      <BottomDock
        paneIds={["terminal"]}
        descriptorFor={descriptorFor}
        renderBody={body}
        onMove={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    expect(getByText("Terminal")).toBeTruthy();
    expect(getByTestId("body-terminal")).toBeTruthy();
    expect(getByTestId("bottom-dock-resize")).toBeTruthy();
  });

  it("forwards the close control to its callback", () => {
    const onClose = vi.fn();
    const { getByLabelText } = render(
      <BottomDock
        paneIds={["terminal"]}
        descriptorFor={descriptorFor}
        renderBody={body}
        onMove={vi.fn()}
        onClose={onClose}
      />,
    );
    fireEvent.click(getByLabelText("Close terminal"));
    expect(onClose).toHaveBeenCalledWith("terminal");
  });
});
