// @vitest-environment jsdom
//
// Behavior contract for the tip-of-the-day modal: shows one tip at a time,
// Previous/Next cycle through them, each shown tip is marked seen, and the
// "Show tips on startup" checkbox reflects and toggles the preference. The
// persistence round-trip (GET /api/tips, mark-seen, show toggle) is covered by
// web/tests/live/tips.spec.ts; this suite is pure prop-driven.

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
  it("opens on the start index and shows that tip with a counter", () => {
    const { getByText } = renderModal({ startIndex: 1 });
    expect(getByText("Second tip")).toBeTruthy();
    expect(getByText("Second body.")).toBeTruthy();
    expect(getByText("Tip 2 of 2")).toBeTruthy();
  });

  it("cycles to the next tip and marks it seen", () => {
    const onMarkSeen = vi.fn();
    const { getByRole, getByText } = renderModal({ onMarkSeen });
    fireEvent.click(getByRole("button", { name: "Next" }));
    expect(getByText("Second tip")).toBeTruthy();
    expect(onMarkSeen).toHaveBeenCalledWith("b");
  });

  it("wraps from the last tip to the first with Next", () => {
    const { getByRole, getByText } = renderModal({ startIndex: 1 });
    fireEvent.click(getByRole("button", { name: "Next" }));
    expect(getByText("First tip")).toBeTruthy();
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

  it("disables navigation and hides the counter for a single tip", () => {
    const { getByRole, queryByText } = renderModal({ tips: [TIPS[0]] });
    expect((getByRole("button", { name: "Next" }) as HTMLButtonElement).disabled).toBe(true);
    expect((getByRole("button", { name: "Previous" }) as HTMLButtonElement).disabled).toBe(true);
    expect(queryByText(/Tip \d+ of/)).toBeNull();
  });

  it("navigates backward with Previous", () => {
    const { getByRole, getByText } = renderModal({ startIndex: 1 });
    fireEvent.click(getByRole("button", { name: "Previous" }));
    expect(getByText("First tip")).toBeTruthy();
    expect(getByText("Tip 1 of 2")).toBeTruthy();
  });

  it("closes from the Close button", () => {
    const { getByRole, onClose } = renderModal();
    fireEvent.click(getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes when the overlay is clicked", () => {
    const { getByRole, onClose } = renderModal();
    fireEvent.click(getByRole("dialog"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes on Escape", () => {
    const { onClose } = renderModal();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("renders an empty state with no tips", () => {
    const { getByText } = renderModal({ tips: [] });
    expect(getByText(/No tips right now/)).toBeTruthy();
  });
});
