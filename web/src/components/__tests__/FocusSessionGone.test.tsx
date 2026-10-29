// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { FocusSessionGone } from "../FocusSessionGone";

describe("FocusSessionGone", () => {
  it("explains the session is gone and exits focus mode from its button", () => {
    const onExit = vi.fn();
    render(<FocusSessionGone onExit={onExit} />);
    expect(screen.getByText("This session no longer exists.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Exit focus mode" }));
    expect(onExit).toHaveBeenCalledTimes(1);
  });
});
