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

  describe("category tabs", () => {
    const mixed = [
      action({ id: "a1", title: "Run thing", group: "Actions" }),
      action({ id: "s1", title: "Save setting", group: "Settings" }),
      action({ id: "sess1", title: "Some session", group: "Sessions" }),
    ];

    it("defaults to All and shows every group's rows (no regression)", () => {
      render(<CommandPalette open onClose={() => {}} actions={mixed} />);
      expect(screen.getByRole("tab", { name: "All" }).getAttribute("aria-selected")).toBe("true");
      expect(screen.getByText("Run thing")).toBeTruthy();
      expect(screen.getByText("Save setting")).toBeTruthy();
      expect(screen.getByText("Some session")).toBeTruthy();
    });

    it("scopes the list to one group when a category tab is clicked", () => {
      render(<CommandPalette open onClose={() => {}} actions={mixed} />);
      fireEvent.click(screen.getByRole("tab", { name: "Settings" }));
      expect(screen.getByText("Save setting")).toBeTruthy();
      expect(screen.queryByText("Run thing")).toBeNull();
      expect(screen.queryByText("Some session")).toBeNull();
    });

    it("offers no tab for an empty category", () => {
      render(<CommandPalette open onClose={() => {}} actions={mixed} />);
      // No Conversations actions and not searching, so no Conversations tab.
      expect(screen.queryByRole("tab", { name: "Conversations" })).toBeNull();
      expect(screen.getByRole("tab", { name: "Settings" })).toBeTruthy();
    });

    it("offers the Conversations tab while a content search is in flight", () => {
      render(<CommandPalette open onClose={() => {}} actions={mixed} searching />);
      expect(screen.getByRole("tab", { name: "Conversations" })).toBeTruthy();
    });

    it("hides the tab strip when only one category has results", () => {
      render(<CommandPalette open onClose={() => {}} actions={[action({ title: "Lonely", group: "Actions" })]} />);
      expect(screen.queryByRole("tablist")).toBeNull();
    });

    it("reflects the active tab's count in the footer", () => {
      render(
        <CommandPalette
          open
          onClose={() => {}}
          actions={[
            action({ id: "a1", title: "One", group: "Actions" }),
            action({ id: "a2", title: "Two", group: "Actions" }),
            action({ id: "s1", title: "Setting", group: "Settings" }),
          ]}
        />,
      );
      expect(screen.getByText("3 actions")).toBeTruthy();
      fireEvent.click(screen.getByRole("tab", { name: "Settings" }));
      expect(screen.getByText("1 action")).toBeTruthy();
    });

    it("cycles tabs with Tab and Shift+Tab", () => {
      render(<CommandPalette open onClose={() => {}} actions={mixed} />);
      const dialog = screen.getByRole("dialog", { name: "Command palette" });
      const selected = () =>
        screen.getAllByRole("tab").find((t) => t.getAttribute("aria-selected") === "true")?.textContent;
      expect(selected()).toBe("All");
      // Tab order is All, Actions, Sessions, Settings (no Conversations here).
      fireEvent.keyDown(dialog, { key: "Tab" });
      expect(selected()).toBe("Actions");
      fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
      expect(selected()).toBe("All");
      fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
      expect(selected()).toBe("Settings");
    });

    it("drops a tab and trims the footer count when a query filters a group out", () => {
      render(
        <CommandPalette
          open
          onClose={() => {}}
          actions={[
            action({ id: "a1", title: "alpha run", group: "Actions" }),
            action({ id: "s1", title: "alpha save", group: "Settings" }),
            action({ id: "sess1", title: "beta sit", group: "Sessions" }),
          ]}
        />,
      );
      expect(screen.getByText("3 actions")).toBeTruthy();
      // "alpha" matches the Actions + Settings rows but not the Sessions row.
      fireEvent.change(screen.getByPlaceholderText("Search actions, sessions, settings…"), {
        target: { value: "alpha" },
      });
      expect(screen.getByText("2 actions")).toBeTruthy();
      expect(screen.queryByRole("tab", { name: "Sessions" })).toBeNull();
      expect(screen.getByRole("tab", { name: "Actions" })).toBeTruthy();
      expect(screen.getByRole("tab", { name: "Settings" })).toBeTruthy();
    });

    it("hides the strip and counts only matches when a query leaves one group", () => {
      render(
        <CommandPalette
          open
          onClose={() => {}}
          actions={[
            action({ id: "a1", title: "Run thing", group: "Actions" }),
            action({ id: "s1", title: "Save setting", group: "Settings" }),
            action({ id: "sess1", title: "Some session", group: "Sessions" }),
          ]}
        />,
      );
      // "setting" only matches the Settings row; the strip collapses (one real
      // category) and the footer reflects the single surviving row.
      fireEvent.change(screen.getByPlaceholderText("Search actions, sessions, settings…"), {
        target: { value: "setting" },
      });
      expect(screen.queryByRole("tablist")).toBeNull();
      expect(screen.getByText("1 action")).toBeTruthy();
      expect(screen.queryByText("Run thing")).toBeNull();
      expect(screen.queryByText("Some session")).toBeNull();
    });

    it("excludes scatter-only fuzzy matches while keeping real matches", () => {
      render(
        <CommandPalette
          open
          onClose={() => {}}
          actions={[
            // "test" only scatter-matches this row (t·e·s·t across the id and
            // keywords), which used to surface it under a short query.
            action({
              id: "action:new-session",
              title: "New session",
              group: "Actions",
              keywords: ["create", "start", "agent", "worktree"],
            }),
            action({ id: "session:x", title: "plugin-host-test", group: "Sessions" }),
          ]}
        />,
      );
      fireEvent.change(screen.getByPlaceholderText("Search actions, sessions, settings…"), {
        target: { value: "test" },
      });
      expect(screen.queryByText("New session")).toBeNull();
      expect(screen.getByText("plugin-host-test")).toBeTruthy();
      expect(screen.getByText("1 action")).toBeTruthy();
    });

    it("resets to All when reopened", () => {
      const { rerender } = render(<CommandPalette open onClose={() => {}} actions={mixed} />);
      fireEvent.click(screen.getByRole("tab", { name: "Settings" }));
      expect(screen.getByRole("tab", { name: "Settings" }).getAttribute("aria-selected")).toBe("true");
      rerender(<CommandPalette open={false} onClose={() => {}} actions={mixed} />);
      rerender(<CommandPalette open onClose={() => {}} actions={mixed} />);
      expect(screen.getByRole("tab", { name: "All" }).getAttribute("aria-selected")).toBe("true");
    });
  });
});
