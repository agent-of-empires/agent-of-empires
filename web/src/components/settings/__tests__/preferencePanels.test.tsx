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
  it("side-by-side and tree toggles write diffViewLayout and diffViewMode", () => {
    const { container } = render(<DiffSettings />);
    const [splitBox, treeBox] = boxes(container);

    expect(splitBox!.checked).toBe(false); // defaults to unified
    fireEvent.click(splitBox!);
    expect(readStored().diffViewLayout).toBe("split");
    fireEvent.click(splitBox!);
    expect(readStored().diffViewLayout).toBe("unified");

    fireEvent.click(treeBox!);
    const mode = readStored().diffViewMode;
    expect(mode === "tree" || mode === "flat").toBe(true);
    fireEvent.click(treeBox!);
    expect(readStored().diffViewMode).toBe(mode === "tree" ? "flat" : "tree");
  });
});

describe("PanelsSettings localStorage contract", () => {
  it("diff and terminal default on, plugins off, and each toggle writes its own key", () => {
    const { container } = render(<PanelsSettings />);
    const [diff, terminal, plugins] = boxes(container);
    expect(boxes(container)).toHaveLength(3);
    expect([diff!.checked, terminal!.checked, plugins!.checked]).toEqual([true, true, false]);

    fireEvent.click(diff!);
    expect(readStored().autoOpenDiffPane).toBe(false);
    expect(readStored().autoOpenTerminalPane).not.toBe(false);
    expect(readStored().autoOpenPluginPanes).toBe(false);

    fireEvent.click(terminal!);
    expect(readStored().autoOpenTerminalPane).toBe(false);

    fireEvent.click(plugins!);
    expect(readStored().autoOpenPluginPanes).toBe(true);

    fireEvent.click(diff!);
    expect(readStored().autoOpenDiffPane).toBe(true);
  });
});
