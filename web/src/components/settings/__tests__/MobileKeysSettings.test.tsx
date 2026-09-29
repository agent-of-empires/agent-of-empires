// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { MobileKeysSettings } from "../MobileKeysSettings";
import { DEFAULT_TOOLBAR_KEYS, MAX_TOOLBAR_KEYS } from "../../../lib/terminalToolbarKeys";

const KEY = "aoe-web-settings";

const stored = () => JSON.parse(window.localStorage.getItem(KEY) ?? "{}") as Record<string, unknown>;
const seed = (settings: Record<string, unknown>) => window.localStorage.setItem(KEY, JSON.stringify(settings));
const rowLabels = () =>
  within(screen.getByRole("list", { name: "Keys in the row" }))
    .getAllByRole("listitem")
    .map((li) => li.textContent?.split(/[↑↓×]/)[0]);

beforeEach(() => {
  cleanup();
  window.localStorage.clear();
});

describe("MobileKeysSettings", () => {
  it("edits the row: remove, reorder, add, and reset", () => {
    render(<MobileKeysSettings />);
    expect(rowLabels()).toEqual(["EscEscape", "TabTab", "CtrlCtrl", "PastePaste from clipboard", "ComposeCompose"]);
    expect(screen.getByLabelText("Move Escape up")).toHaveProperty("disabled", true);

    fireEvent.click(screen.getByLabelText("Remove Paste from clipboard"));
    fireEvent.click(screen.getByLabelText("Move Compose up"));
    fireEvent.click(screen.getByLabelText("Add Enter"));
    expect(stored().mobileToolbarKeys).toEqual(["esc", "tab", "compose", "ctrl", "enter"]);

    fireEvent.click(screen.getByRole("button", { name: "Reset to default" }));
    expect(stored().mobileToolbarKeys).toEqual(DEFAULT_TOOLBAR_KEYS);
  });

  it("caps the row, and drops unknown or repeated stored ids", () => {
    seed({ mobileToolbarKeys: ["esc", "bogus", "esc", "tab", "ctrl", "enter", "home", "end", "ctrl-c", "ctrl-o"] });
    render(<MobileKeysSettings />);
    expect(rowLabels()).toHaveLength(MAX_TOOLBAR_KEYS);
    expect(screen.getByLabelText("Add Paste from clipboard")).toHaveProperty("disabled", true);
  });

  it("toggles the arrow joystick", () => {
    render(<MobileKeysSettings />);
    const toggle = screen.getByRole("checkbox", { name: /Arrow joystick/ });
    expect(toggle).toHaveProperty("checked", true);
    fireEvent.click(toggle);
    expect(stored().showArrowJoystick).toBe(false);
  });
});
