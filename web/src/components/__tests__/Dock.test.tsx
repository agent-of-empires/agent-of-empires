// @vitest-environment jsdom
//
// Contract test for the Dock + PaneFrame chrome: one framed pane per id, the
// resize handle only when two panes share the dock, and the move/close
// controls wired to their callbacks.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { Dock } from "../Dock";
import { BUILTIN_PANES } from "../../lib/panes";

afterEach(() => cleanup());

const body = (id: string) => <div data-testid={`body-${id}`}>{id}</div>;
const descriptorFor = (id: string) => {
  const d = BUILTIN_PANES.find((p) => p.id === id)!;
  return { title: d.title, icon: d.icon };
};

describe("Dock", () => {
  it("frames each open pane with its registry title", () => {
    const { getByText, getByTestId } = render(
      <Dock
        location="right"
        paneIds={["diff", "terminal"]}
        descriptorFor={descriptorFor}
        renderBody={body}
        onMove={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    expect(getByText("Diff")).toBeTruthy();
    expect(getByText("Terminal")).toBeTruthy();
    expect(getByTestId("body-diff")).toBeTruthy();
    // Two panes share the dock, so the divider is present.
    expect(getByTestId("dock-resize-right")).toBeTruthy();
  });

  it("omits the resize handle with a single open pane", () => {
    const { queryByTestId } = render(
      <Dock
        location="bottom"
        paneIds={["terminal"]}
        descriptorFor={descriptorFor}
        renderBody={body}
        onMove={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    expect(queryByTestId("dock-resize-bottom")).toBeNull();
  });

  it("move and close controls fire with the pane id and target dock", () => {
    const onMove = vi.fn();
    const onClose = vi.fn();
    const { getByLabelText } = render(
      <Dock
        location="right"
        paneIds={["diff"]}
        descriptorFor={descriptorFor}
        renderBody={body}
        onMove={onMove}
        onClose={onClose}
      />,
    );
    // Right-docked pane moves to the bottom dock.
    fireEvent.click(getByLabelText("Move diff to bottom dock"));
    expect(onMove).toHaveBeenCalledWith("diff", "bottom");
    fireEvent.click(getByLabelText("Close diff"));
    expect(onClose).toHaveBeenCalledWith("diff");
  });
});
