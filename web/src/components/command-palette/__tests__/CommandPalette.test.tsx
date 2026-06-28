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

  it("shows a spinner row in Conversations while a content search runs", () => {
    render(<CommandPalette open onClose={() => {}} actions={[action()]} searching />);
    expect(screen.getByText("Searching conversations…")).toBeTruthy();
  });

  it("keeps conversation hits even when the query does not match their text", () => {
    render(
      <CommandPalette
        open
        onClose={() => {}}
        actions={[
          action({ id: "session:s1", title: "Some Title", group: "Sessions" }),
          action({ id: "conversation:s2", title: "Hit session", group: "Conversations" }),
        ]}
      />,
    );
    // Type a query that matches neither title; the conversation hit is
    // force-kept (server already matched it by content), the metadata
    // session row is filtered out.
    fireEvent.change(screen.getByPlaceholderText("Search actions, sessions, settings…"), {
      target: { value: "zzzznomatch" },
    });
    expect(screen.getByText("Hit session")).toBeTruthy();
    expect(screen.queryByText("Some Title")).toBeNull();
  });

  it("reports the typed query through onSearchChange", () => {
    const onSearchChange = vi.fn();
    render(<CommandPalette open onClose={() => {}} actions={[action()]} onSearchChange={onSearchChange} />);
    fireEvent.change(screen.getByPlaceholderText("Search actions, sessions, settings…"), {
      target: { value: "reconciler" },
    });
    expect(onSearchChange).toHaveBeenCalledWith("reconciler");
  });
});
