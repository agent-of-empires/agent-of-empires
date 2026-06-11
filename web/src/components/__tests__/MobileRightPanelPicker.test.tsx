// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { MobileRightPanelPicker } from "../MobileRightPanelPicker";

function setup(overrides: Partial<Parameters<typeof MobileRightPanelPicker>[0]> = {}) {
  const onSelect = vi.fn();
  const onClose = vi.fn();
  render(<MobileRightPanelPicker open active="agent" onSelect={onSelect} onClose={onClose} {...overrides} />);
  return { onSelect, onClose };
}

describe("MobileRightPanelPicker", () => {
  it("renders nothing when closed", () => {
    const { container } = render(
      <MobileRightPanelPicker open={false} active="agent" onSelect={vi.fn()} onClose={vi.fn()} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders the three view entries with the active one marked", () => {
    setup({ active: "paired" });
    expect(screen.getByTestId("mobile-right-panel-picker")).toBeDefined();
    expect(screen.getByTestId("mobile-right-panel-pick-agent")).toBeDefined();
    expect(screen.getByTestId("mobile-right-panel-pick-diff")).toBeDefined();
    const paired = screen.getByTestId("mobile-right-panel-pick-paired");
    expect(paired.getAttribute("aria-current")).toBe("true");
  });

  it("calls onSelect with the chosen view", () => {
    const { onSelect } = setup();
    fireEvent.click(screen.getByTestId("mobile-right-panel-pick-diff"));
    expect(onSelect).toHaveBeenCalledWith("diff");
  });

  it("closes on backdrop click", () => {
    const { onClose } = setup();
    fireEvent.click(screen.getByTestId("mobile-right-panel-picker-backdrop"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes on Escape", () => {
    const { onClose } = setup();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("ignores non-Escape keys", () => {
    const { onClose } = setup();
    fireEvent.keyDown(window, { key: "Enter" });
    expect(onClose).not.toHaveBeenCalled();
  });
});
