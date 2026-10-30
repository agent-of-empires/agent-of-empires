// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { FocusSessionGone } from "../FocusSessionGone";

describe("FocusSessionGone", () => {
  afterEach(cleanup);

  it.each([
    ["Mongols", true, "Mongols was moved to Trash."],
    ["Mongols", false, "Mongols no longer exists."],
    [null, false, "This Session no longer exists."],
  ])("names %s (trashed: %s)", (title, trashed, text) => {
    const onExit = vi.fn();
    render(<FocusSessionGone title={title} trashed={trashed} onExit={onExit} />);
    expect(screen.getByTestId("focus-session-gone").querySelector("p")!.textContent).toBe(text);
    fireEvent.click(screen.getByRole("button", { name: "Show all Sessions" }));
    expect(onExit).toHaveBeenCalledTimes(1);
  });
});
