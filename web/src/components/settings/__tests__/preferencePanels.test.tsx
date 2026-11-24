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
  it("file editing defaults off and writes its own key", () => {
    const { container } = render(<PanelsSettings />);
    const [editing] = boxes(container);
    expect(boxes(container)).toHaveLength(1);
    expect(editing!.checked).toBe(false);
    fireEvent.click(editing!);
    expect(readStored().fileEditing).toBe(true);
    fireEvent.click(editing!);
    expect(readStored().fileEditing).toBe(false);
  });
});
