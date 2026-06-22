// @vitest-environment jsdom
//
// Behavior contract for the dashboard TipsModal: unseen tips lead, seen tips
// collapse, expanding an unseen tip marks it seen, and the footer actions fire.
// The end-to-end persistence round-trip (GET /api/tips, mark-seen, disable) is
// covered by web/tests/live/tips.spec.ts; this suite is pure prop-driven.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { TipsModal } from "../TipsModal";
import type { TipDto } from "../../lib/api";

afterEach(() => {
  cleanup();
});

const TIPS: TipDto[] = [
  { id: "install-dashboard-pwa", title: "Install the dashboard as an app", body: "PWA body.", seen: false },
  { id: "old-one", title: "An old tip", body: "Seen body.", seen: true },
];

function renderModal(overrides: { tips?: TipDto[]; onMarkSeen?: () => void; onDisable?: () => void } = {}) {
  const onClose = vi.fn();
  const onMarkSeen = overrides.onMarkSeen ?? vi.fn();
  const onDisable = overrides.onDisable ?? vi.fn();
  const utils = render(
    <TipsModal tips={overrides.tips ?? TIPS} onMarkSeen={onMarkSeen} onDisable={onDisable} onClose={onClose} />,
  );
  return { ...utils, onClose, onMarkSeen, onDisable };
}

describe("TipsModal", () => {
  it("shows unseen tips and hides seen ones behind a collapsed section", () => {
    const { getByText, queryByText } = renderModal();
    expect(getByText("Install the dashboard as an app")).toBeTruthy();
    // Seen tip is behind the collapsed "Show seen (1)" toggle.
    expect(queryByText("An old tip")).toBeNull();
    expect(getByText("Show seen (1)")).toBeTruthy();
  });

  it("expands the seen section on demand", () => {
    const { getByText, queryByText } = renderModal();
    fireEvent.click(getByText("Show seen (1)"));
    expect(getByText("An old tip")).toBeTruthy();
    expect(queryByText("Hide seen (1)")).toBeTruthy();
  });

  it("marks an unseen tip seen when it is expanded", () => {
    const onMarkSeen = vi.fn();
    const { getByText } = renderModal({ onMarkSeen });
    fireEvent.click(getByText("Install the dashboard as an app"));
    expect(getByText("PWA body.")).toBeTruthy();
    expect(onMarkSeen).toHaveBeenCalledWith("install-dashboard-pwa");
  });

  it("does not re-mark an already-seen tip when expanded", () => {
    const onMarkSeen = vi.fn();
    const { getByText } = renderModal({ onMarkSeen });
    fireEvent.click(getByText("Show seen (1)"));
    fireEvent.click(getByText("An old tip"));
    expect(onMarkSeen).not.toHaveBeenCalled();
  });

  it("disables tips and closes from Don't show again", () => {
    const { getByText, onDisable, onClose } = renderModal();
    fireEvent.click(getByText("Don't show again"));
    expect(onDisable).toHaveBeenCalledTimes(1);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("renders an empty state when there are no tips", () => {
    const { getByText } = renderModal({ tips: [] });
    expect(getByText(/No tips right now/)).toBeTruthy();
  });
});
