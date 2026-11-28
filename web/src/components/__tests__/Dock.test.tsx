// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";

import { Dock } from "../Dock";
import { PaneDndStateContext } from "../paneDnd";
import { BUILTIN_PANES } from "../../lib/panes";

const body = (id: string) => <div data-testid={`body-${id}`}>{id}</div>;
const descriptorFor = (id: string) => {
  // Tests only feed built-in tab ids; resolve "terminal:s1:0" to the terminal pane.
  const kind = id.startsWith("terminal") ? "terminal" : id;
  const d = BUILTIN_PANES.find((p) => p.id === kind)!;
  return { title: d.title, icon: d.icon };
};

function renderDock(props: Partial<React.ComponentProps<typeof Dock>> = {}) {
  return render(
    <Dock
      location="right"
      groupIndex={0}
      tabs={["diff", "terminal:s1:0"]}
      active="terminal:s1:0"
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

describe("Dock", () => {
  it("renders one tab per id but only the active body", () => {
    const { getByTestId, queryByTestId } = renderDock();
    expect(getByTestId("pane-tab-diff")).toBeTruthy();
    expect(getByTestId("pane-tab-terminal:s1:0")).toBeTruthy();
    // Only the active tab's body is mounted.
    expect(getByTestId("body-terminal:s1:0")).toBeTruthy();
    expect(queryByTestId("body-diff")).toBeNull();
  });

  it("routes tab clicks, close, move, and new-terminal controls to their callbacks", () => {
    const onActivate = vi.fn();
    const onClose = vi.fn();
    const onMove = vi.fn();
    const onNewTerminal = vi.fn();
    const { getByTestId, getByLabelText } = renderDock({ onActivate, onClose, onMove, onNewTerminal });
    fireEvent.click(getByTestId("pane-tab-diff"));
    expect(onActivate).toHaveBeenCalledWith("diff");
    fireEvent.click(getByLabelText("Close diff"));
    expect(onClose).toHaveBeenCalledWith("diff");
    fireEvent.click(getByLabelText("Move terminal to bottom dock"));
    expect(onMove).toHaveBeenCalledWith("terminal:s1:0", "bottom");
    fireEvent.click(getByLabelText("New terminal"));
    expect(onNewTerminal).toHaveBeenCalled();
  });

  it("keeps terminal controls to a group of terminals and maximize to the dock's first group", () => {
    const maximize = { maximized: false, onToggle: vi.fn() };
    const review = renderDock({
      tabs: ["diff"],
      active: "diff",
      maximize,
      terminalSwitcher: <i data-testid="switcher" />,
    });
    expect([review.queryByLabelText("New terminal"), review.queryByTestId("switcher")]).toEqual([null, null]);
    expect(review.getByTestId("dock-maximize")).toBeTruthy();
    review.unmount();

    const terminals = renderDock({
      groupIndex: 1,
      tabs: ["terminal:s1:0"],
      active: "terminal:s1:0",
      maximize,
      terminalSwitcher: <i data-testid="switcher" />,
    });
    expect([!!terminals.getByLabelText("New terminal"), !!terminals.getByTestId("switcher")]).toEqual([true, true]);
    expect(terminals.queryByTestId("dock-maximize")).toBeNull();
  });

  it("shows split drop zones only while a tab is being dragged", () => {
    // Baseline Dock with no drag in progress: no split zones mounted (assert
    // after mounting so this catches unconditional rendering, not an empty DOM).
    const { unmount } = renderDock();
    expect(document.querySelector('[data-testid="pane-split-right-0-before"]')).toBeNull();
    expect(document.querySelector('[data-testid="pane-split-right-0-after"]')).toBeNull();
    unmount();

    render(
      <PaneDndStateContext.Provider
        value={{ activeTab: "diff", source: { dock: "right", group: 0 }, dropTarget: null }}
      >
        <Dock
          location="right"
          groupIndex={0}
          tabs={["diff", "terminal:s1:0"]}
          active="terminal:s1:0"
          descriptorFor={descriptorFor}
          renderBody={body}
          onActivate={vi.fn()}
          onClose={vi.fn()}
          onMove={vi.fn()}
        />
      </PaneDndStateContext.Provider>,
    );
    expect(document.querySelector('[data-testid="pane-split-right-0-before"]')).toBeTruthy();
    expect(document.querySelector('[data-testid="pane-split-right-0-after"]')).toBeTruthy();
  });
});
