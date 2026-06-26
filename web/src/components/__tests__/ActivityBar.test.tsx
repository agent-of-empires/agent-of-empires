// @vitest-environment jsdom
import { afterEach } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ActivityBar } from "../ActivityBar";

afterEach(() => cleanup());

describe("ActivityBar", () => {
  it("renders one toggle per built-in pane and reflects open state", () => {
    const { getByTestId } = render(
      <ActivityBar
        layout={{ diff: { open: true, dock: "right" }, terminal: { open: false, dock: "right" } }}
        onToggle={vi.fn()}
      />,
    );
    expect(getByTestId("pane-toggle-diff").getAttribute("aria-pressed")).toBe("true");
    expect(getByTestId("pane-toggle-terminal").getAttribute("aria-pressed")).toBe("false");
  });

  it("calls onToggle with the pane id on click", () => {
    const onToggle = vi.fn();
    const { getByTestId } = render(
      <ActivityBar
        layout={{ diff: { open: true, dock: "right" }, terminal: { open: true, dock: "right" } }}
        onToggle={onToggle}
      />,
    );
    fireEvent.click(getByTestId("pane-toggle-terminal"));
    expect(onToggle).toHaveBeenCalledWith("terminal");
  });
});
