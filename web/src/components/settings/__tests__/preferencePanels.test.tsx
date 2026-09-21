// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { DiffSettings } from "../DiffSettings";
import { PanelsSettings } from "../PanelsSettings";

function readStored(): Record<string, unknown> {
  const raw = window.localStorage.getItem("aoe-web-settings");
  return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
}

const boxes = (container: HTMLElement) =>
  Array.from(container.querySelectorAll("input[type=checkbox]")) as HTMLInputElement[];

beforeEach(() => {
  window.localStorage.clear();
});

describe("DiffSettings localStorage contract", () => {
  it("toggling side-by-side writes diffViewLayout", () => {
    const { getByText, container } = render(<DiffSettings />);
    const splitBox = boxes(container)[0]!;

    expect(getByText("Side-by-side diff")).toBeTruthy();
    expect(splitBox.checked).toBe(false); // defaults to unified

    fireEvent.click(splitBox);
    expect(readStored().diffViewLayout).toBe("split");

    fireEvent.click(splitBox);
    expect(readStored().diffViewLayout).toBe("unified");
  });

  it("toggling tree file list writes diffViewMode", () => {
    const { getByText, container } = render(<DiffSettings />);
    const treeBox = boxes(container)[1]!;

    expect(getByText("Tree file list")).toBeTruthy();
    fireEvent.click(treeBox);
    const mode = readStored().diffViewMode;
    expect(mode === "tree" || mode === "flat").toBe(true);
    // Flipping again yields the opposite value.
    fireEvent.click(treeBox);
    expect(readStored().diffViewMode).toBe(mode === "tree" ? "flat" : "tree");
  });
});

describe("PanelsSettings localStorage contract", () => {
  it("diff and terminal toggles default on, plugin toggle defaults off", () => {
    const { container } = render(<PanelsSettings />);
    const [diff, terminal, plugins] = boxes(container);
    expect(boxes(container)).toHaveLength(3);
    expect([diff!.checked, terminal!.checked, plugins!.checked]).toEqual([true, true, false]);
  });

  it("each toggle writes its own key independently", () => {
    const { container } = render(<PanelsSettings />);
    const [diff, terminal, plugins] = boxes(container);

    fireEvent.click(diff!);
    expect(readStored().autoOpenDiffPane).toBe(false);
    expect(readStored().autoOpenTerminalPane).not.toBe(false);
    expect(readStored().autoOpenPluginPanes).toBe(false);

    fireEvent.click(terminal!);
    expect(readStored().autoOpenTerminalPane).toBe(false);

    fireEvent.click(plugins!);
    expect(readStored().autoOpenPluginPanes).toBe(true);

    // Flipping back restores true.
    fireEvent.click(diff!);
    expect(readStored().autoOpenDiffPane).toBe(true);
  });
});
