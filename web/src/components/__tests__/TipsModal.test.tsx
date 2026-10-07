// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { TipsModal } from "../TipsModal";
import type { TipDto } from "../../lib/api";

afterEach(() => {
  cleanup();
});

const TIPS: TipDto[] = [
  { id: "a", title: "First tip", body: "First body.", seen: false },
  { id: "b", title: "Second tip", body: "Second body.", seen: false },
];

function renderModal(
  overrides: {
    tips?: TipDto[];
    startIndex?: number;
    enabled?: boolean;
    onSetEnabled?: () => void;
    onMarkSeen?: () => void;
  } = {},
) {
  const onClose = vi.fn();
  const onMarkSeen = overrides.onMarkSeen ?? vi.fn();
  const onSetEnabled = overrides.onSetEnabled ?? vi.fn();
  const utils = render(
    <TipsModal
      tips={overrides.tips ?? TIPS}
      startIndex={overrides.startIndex ?? 0}
      enabled={overrides.enabled ?? true}
      onMarkSeen={onMarkSeen}
      onSetEnabled={onSetEnabled}
      onClose={onClose}
    />,
  );
  return { ...utils, onClose, onMarkSeen, onSetEnabled };
}

describe("TipsModal (tip of the day)", () => {
  it("opens on the start index and navigates with wraparound, marking shown tips seen", () => {
    const onMarkSeen = vi.fn();
    const { getByRole, getByText } = renderModal({ startIndex: 1, onMarkSeen });
    expect(getByText("Second tip")).toBeTruthy();
    expect(getByText("Second body.")).toBeTruthy();
    expect(getByText("Tip 2 of 2")).toBeTruthy();

    fireEvent.click(getByRole("button", { name: "Next" }));
    expect(getByText("First tip")).toBeTruthy();
    expect(getByText("Tip 1 of 2")).toBeTruthy();
    expect(onMarkSeen).toHaveBeenLastCalledWith("a");

    fireEvent.click(getByRole("button", { name: "Next" }));
    expect(getByText("Second tip")).toBeTruthy();
    expect(onMarkSeen).toHaveBeenLastCalledWith("b");

    fireEvent.click(getByRole("button", { name: "Previous" }));
    expect(getByText("Tip 1 of 2")).toBeTruthy();
  });

  it("reflects the enabled state and toggles it from the checkbox", () => {
    const onSetEnabled = vi.fn();
    const { getByRole } = renderModal({ enabled: true, onSetEnabled });
    const checkbox = getByRole("checkbox", { name: "Show tips on startup" }) as HTMLInputElement;
    expect(checkbox.checked).toBe(true);
    fireEvent.click(checkbox);
    expect(onSetEnabled).toHaveBeenCalledWith(false);
  });

  it("disables navigation for a single tip and shows an empty state with none", () => {
    const one = renderModal({ tips: [TIPS[0]] });
    expect((one.getByRole("button", { name: "Next" }) as HTMLButtonElement).disabled).toBe(true);
    expect((one.getByRole("button", { name: "Previous" }) as HTMLButtonElement).disabled).toBe(true);
    expect(one.queryByText(/Tip \d+ of/)).toBeNull();
    one.unmount();
    const { getByText } = renderModal({ tips: [] });
    expect(getByText(/No tips right now/)).toBeTruthy();
  });

  it("closes from the Close button, the overlay, and Escape", () => {
    const { getByRole, onClose } = renderModal();
    fireEvent.click(getByRole("button", { name: "Close" }));
    fireEvent.click(getByRole("dialog"));
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(3);
  });
});
