// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { MainViewTabs } from "../MainViewTabs";

function setup(fileShown: boolean) {
  const handlers = { onShowAgent: vi.fn(), onShowFile: vi.fn(), onCloseFile: vi.fn() };
  render(<MainViewTabs filePath="src/lib/example.ts" fileShown={fileShown} {...handlers} />);
  return handlers;
}

describe("MainViewTabs", () => {
  it.each([true, false])("marks the shown tab (file shown: %s) and names the file by its base name", (fileShown) => {
    setup(fileShown);
    expect(screen.getByRole("tab", { name: "Agent" }).getAttribute("aria-selected")).toBe(String(!fileShown));
    const file = screen.getByRole("tab", { name: "example.ts" });
    expect(file.getAttribute("aria-selected")).toBe(String(fileShown));
    expect(file.getAttribute("title")).toBe("src/lib/example.ts");
  });

  it("switches between the Agent and the file, and closes the file", () => {
    const { onShowAgent, onShowFile, onCloseFile } = setup(true);
    fireEvent.click(screen.getByRole("tab", { name: "Agent" }));
    fireEvent.click(screen.getByRole("tab", { name: "example.ts" }));
    fireEvent.click(screen.getByRole("button", { name: "Close example.ts" }));
    expect([onShowAgent, onShowFile, onCloseFile].map((f) => f.mock.calls.length)).toEqual([1, 1, 1]);
  });
});
