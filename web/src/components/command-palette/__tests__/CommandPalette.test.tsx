// @vitest-environment jsdom
//
// Coverage for CommandPalette: closed renders nothing, open renders a modal
// dialog grouped by GROUP_ORDER, selecting an item closes and performs the
// action (via queueMicrotask), the backdrop closes, and the footer shows the
// action count.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { CommandPalette } from "../CommandPalette";
import type { CommandAction } from "../types";

function action(over: Partial<CommandAction> = {}): CommandAction {
  return { id: "a1", title: "Do thing", group: "Actions", perform: () => {}, ...over };
}

afterEach(cleanup);

describe("CommandPalette", () => {
  it("renders nothing when closed", () => {
    const { container } = render(<CommandPalette open={false} onClose={() => {}} actions={[]} />);
    expect(container.firstChild).toBeNull();
  });

  it("renders a modal dialog with grouped actions when open", () => {
    render(
      <CommandPalette
        open
        onClose={() => {}}
        actions={[
          action({ id: "a1", title: "Run", group: "Actions" }),
          action({ id: "s1", title: "Save", group: "Settings" }),
        ]}
      />,
    );
    expect(screen.getByRole("dialog", { name: "Command palette" })).toBeTruthy();
    expect(screen.getByText("Run")).toBeTruthy();
    expect(screen.getByText("Save")).toBeTruthy();
    expect(screen.getByText("2 actions")).toBeTruthy();
  });

  it("singularizes the footer count", () => {
    render(<CommandPalette open onClose={() => {}} actions={[action()]} />);
    expect(screen.getByText("1 action")).toBeTruthy();
  });

  it("closes and performs the action on select", async () => {
    const onClose = vi.fn();
    const perform = vi.fn();
    render(<CommandPalette open onClose={onClose} actions={[action({ title: "Launch", perform })]} />);
    fireEvent.click(screen.getByText("Launch"));
    expect(onClose).toHaveBeenCalledOnce();
    await Promise.resolve();
    expect(perform).toHaveBeenCalledOnce();
  });

  it("closes when the backdrop is clicked", () => {
    const onClose = vi.fn();
    render(<CommandPalette open onClose={onClose} actions={[action()]} />);
    fireEvent.click(screen.getByTestId("command-palette-backdrop"));
    expect(onClose).toHaveBeenCalledOnce();
  });
});
