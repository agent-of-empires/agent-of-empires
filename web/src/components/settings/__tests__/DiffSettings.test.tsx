// @vitest-environment jsdom
//
// Contract test for the DiffSettings panel. Like TerminalSettings (and unlike
// the server-backed panels under settings/), this persists through
// useWebSettings + localStorage (key `aoe-web-settings`), not PATCH
// /api/settings. The contract is the JSON shape written to that key.

import { beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { DiffSettings } from "../DiffSettings";

const KEY = "aoe-web-settings";

function readStored(): Record<string, unknown> {
  const raw = window.localStorage.getItem(KEY);
  return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
}

beforeEach(() => {
  window.localStorage.clear();
});

describe("DiffSettings localStorage contract", () => {
  it("toggling side-by-side writes diffViewLayout", () => {
    const { getByText, container } = render(<DiffSettings />);
    const splitBox = container.querySelectorAll(
      "input[type=checkbox]",
    )[0] as HTMLInputElement;

    expect(getByText("Side-by-side diff")).toBeTruthy();
    expect(splitBox.checked).toBe(false); // defaults to unified

    fireEvent.click(splitBox);
    expect(readStored().diffViewLayout).toBe("split");

    fireEvent.click(splitBox);
    expect(readStored().diffViewLayout).toBe("unified");
  });

  it("toggling tree file list writes diffViewMode", () => {
    const { getByText, container } = render(<DiffSettings />);
    const treeBox = container.querySelectorAll(
      "input[type=checkbox]",
    )[1] as HTMLInputElement;

    expect(getByText("Tree file list")).toBeTruthy();
    fireEvent.click(treeBox);
    const mode = readStored().diffViewMode;
    expect(mode === "tree" || mode === "flat").toBe(true);
    // Flipping again yields the opposite value.
    fireEvent.click(treeBox);
    expect(readStored().diffViewMode).toBe(mode === "tree" ? "flat" : "tree");
  });
});
