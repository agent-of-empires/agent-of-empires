// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { MobileRightPanelPicker } from "../MobileRightPanelPicker";
import { FileDiff, Puzzle } from "lucide-react";
import { makeSession } from "./fixtures";

const PLUGIN_ID = "plugin:acme.kit:gh";
const describePane = (id: string) =>
  id === PLUGIN_ID ? { title: "GitHub", icon: Puzzle } : { title: id, icon: FileDiff };

function setup(overrides: Partial<Parameters<typeof MobileRightPanelPicker>[0]> = {}) {
  const onSelect = vi.fn();
  const onClose = vi.fn();
  const onPickShell = vi.fn();
  render(
    <MobileRightPanelPicker
      open
      active="agent"
      sessionTitle="s1"
      availablePanes={["diff"]}
      describePane={describePane}
      shellGroups={[]}
      onPickShell={onPickShell}
      shownShell={null}
      onSelect={onSelect}
      onClose={onClose}
      {...overrides}
    />,
  );
  return { onSelect, onClose, onPickShell };
}

describe("MobileRightPanelPicker", () => {
  it("lists every live paired shell and picks one", () => {
    const other = makeSession({ id: "s2", title: "Other", is_sandboxed: true });
    const shell = { sessionId: "s2", index: 1, mode: "container" as const, open: false };
    const { onPickShell } = setup({ shellGroups: [{ session: other, shells: [shell] }] });
    const row = screen.getByTestId("mobile-shell-pick-s2-1-container");
    expect(row.textContent).toContain("Other · 2");
    expect(row.textContent).toContain("Container");
    fireEvent.click(row);
    expect(onPickShell).toHaveBeenCalledWith(shell);
  });

  it("marks only the shell the paired view shows, not every shell with a desktop tab", () => {
    const a = makeSession({ id: "a", title: "A" });
    const b = makeSession({ id: "b", title: "B" });
    setup({
      active: "paired",
      shellGroups: [
        { session: a, shells: [{ sessionId: "a", index: 0, mode: "host", open: true }] },
        { session: b, shells: [{ sessionId: "b", index: 0, mode: "host", open: true }] },
      ],
      shownShell: { sessionId: "b", index: 0, mode: "host" },
    });
    expect(
      ["a", "b"].map((id) => screen.getByTestId(`mobile-shell-pick-${id}-0-host`).getAttribute("aria-current")),
    ).toEqual([null, "true"]);
  });

  it("hides gated entries not in availablePanes and marks the active one", () => {
    setup({ availablePanes: [], active: "paired" });
    expect(screen.queryByTestId("mobile-right-panel-pick-agents")).toBeNull();
    expect(screen.queryByTestId("mobile-right-panel-pick-diff")).toBeNull();
    expect(screen.queryByTestId("mobile-right-panel-pick-files")).toBeNull();
    // The mobile-only pseudo-views are never gated.
    expect(screen.getByTestId("mobile-right-panel-pick-agent")).toBeDefined();
    expect(screen.getByTestId("mobile-right-panel-pick-paired").getAttribute("aria-current")).toBe("true");
  });

  it("shows the Background and Files entries and selects them when available", () => {
    const { onSelect } = setup({ availablePanes: ["diff", "files", "agents"] });
    fireEvent.click(screen.getByTestId("mobile-right-panel-pick-agents"));
    expect(onSelect).toHaveBeenCalledWith("agents");
    fireEvent.click(screen.getByTestId("mobile-right-panel-pick-files"));
    expect(onSelect).toHaveBeenCalledWith("files");
  });

  it("lists plugin panes with their descriptor title and selects them by id", () => {
    const { onSelect } = setup({ availablePanes: ["diff", PLUGIN_ID], active: PLUGIN_ID });
    const option = screen.getByTestId("mobile-right-panel-pick-plugin:acme.kit:gh");
    expect(option.textContent).toContain("GitHub");
    expect(option.getAttribute("aria-current")).toBe("true");
    fireEvent.click(option);
    expect(onSelect).toHaveBeenCalledWith("plugin:acme.kit:gh");
  });

  it("closes on backdrop click and on Escape only", () => {
    const { onClose } = setup();
    fireEvent.keyDown(window, { key: "Enter" });
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByTestId("mobile-right-panel-picker-backdrop"));
    expect(onClose).toHaveBeenCalledTimes(2);
  });
});
