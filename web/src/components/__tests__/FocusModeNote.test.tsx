// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { FocusModeNote } from "../FocusModeNote";

describe("FocusModeNote", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  it("names the session until dismissed, then stays dismissed in later windows", () => {
    const first = render(<FocusModeNote title="Mongols" />);
    expect(screen.getByTestId("focus-mode-note").textContent).toBe(
      "Only Mongols shows here. Other Sessions still notify through your system.×",
    );
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByTestId("focus-mode-note")).toBeNull();
    first.unmount();

    render(<FocusModeNote title="Goths" />);
    expect(screen.queryByTestId("focus-mode-note")).toBeNull();
  });
});
