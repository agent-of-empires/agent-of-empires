// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { CommandPalette } from "../CommandPalette";
import type { CommandAction } from "../types";

function action(over: Partial<CommandAction> = {}): CommandAction {
  return { id: "a1", title: "Do thing", group: "Actions", perform: () => {}, ...over };
}

const mixed = [
  action({ id: "a1", title: "Run thing", group: "Actions" }),
  action({ id: "s1", title: "Save setting", group: "Settings" }),
  action({ id: "sess1", title: "Some session", group: "Sessions" }),
];

function open(actions: CommandAction[], props: Partial<Parameters<typeof CommandPalette>[0]> = {}) {
  return render(<CommandPalette open onClose={() => {}} actions={actions} {...props} />);
}

const type = (value: string) =>
  fireEvent.change(screen.getByPlaceholderText("Search actions, sessions, settings…"), { target: { value } });
const selectedTab = () =>
  screen.getAllByRole("tab").find((t) => t.getAttribute("aria-selected") === "true")?.textContent;

afterEach(cleanup);

describe("CommandPalette", () => {
  it("renders a modal dialog with every group's rows and a pluralized count", () => {
    const { rerender } = open(mixed);
    expect(screen.getByRole("dialog", { name: "Command palette" })).toBeTruthy();
    for (const t of ["Run thing", "Save setting", "Some session"]) expect(screen.getByText(t)).toBeTruthy();
    expect(screen.getByText("3 actions")).toBeTruthy();
    expect(selectedTab()).toBe("All");
    rerender(<CommandPalette open onClose={() => {}} actions={[action()]} />);
    expect(screen.getByText("1 action")).toBeTruthy();
  });

  it("closes and performs the action on select", async () => {
    const onClose = vi.fn();
    const perform = vi.fn();
    open([action({ title: "Launch", perform })], { onClose });
    fireEvent.click(screen.getByText("Launch"));
    expect(onClose).toHaveBeenCalledOnce();
    await Promise.resolve();
    expect(perform).toHaveBeenCalledOnce();
  });

  it("closes when the backdrop is clicked", () => {
    const onClose = vi.fn();
    open([action()], { onClose });
    fireEvent.click(screen.getByTestId("command-palette-backdrop"));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("keeps conversation hits even when the query does not match their text", () => {
    open([
      action({ id: "session:s1", title: "Some Title", group: "Sessions" }),
      action({ id: "conversation:s2", title: "Hit session", group: "Conversations" }),
    ]);
    type("zzzznomatch");
    expect(screen.getByText("Hit session")).toBeTruthy();
    expect(screen.queryByText("Some Title")).toBeNull();
  });

  describe("category tabs", () => {
    it("scopes the list and footer count to the clicked tab, with no tab for empty groups", () => {
      open([...mixed, action({ id: "s2", title: "Other setting", group: "Settings" })]);
      expect(screen.queryByRole("tab", { name: "Conversations" })).toBeNull();
      fireEvent.click(screen.getByRole("tab", { name: "Settings" }));
      expect(screen.getByText("Save setting")).toBeTruthy();
      expect(screen.queryByText("Run thing")).toBeNull();
      expect(screen.queryByText("Some session")).toBeNull();
      expect(screen.getByText("2 actions")).toBeTruthy();
    });

    it("cycles tabs with Tab and Shift+Tab", () => {
      open(mixed);
      const dialog = screen.getByRole("dialog", { name: "Command palette" });
      fireEvent.keyDown(dialog, { key: "Tab" });
      expect(selectedTab()).toBe("Actions");
      fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
      expect(selectedTab()).toBe("All");
      fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
      expect(selectedTab()).toBe("Settings");
    });

    it("hides the strip and counts only matches when a query leaves one group", () => {
      open(mixed);
      type("setting");
      expect(screen.queryByRole("tablist")).toBeNull();
      expect(screen.getByText("1 action")).toBeTruthy();
      expect(screen.queryByText("Run thing")).toBeNull();
    });

    it("excludes scatter-only fuzzy matches while keeping real matches", () => {
      open([
        action({
          id: "action:new-session",
          title: "New session",
          group: "Actions",
          keywords: ["create", "start", "agent", "worktree"],
        }),
        action({ id: "session:x", title: "plugin-host-test", group: "Sessions" }),
      ]);
      type("test");
      expect(screen.queryByText("New session")).toBeNull();
      expect(screen.getByText("plugin-host-test")).toBeTruthy();
      expect(screen.getByText("1 action")).toBeTruthy();
    });
  });
});
