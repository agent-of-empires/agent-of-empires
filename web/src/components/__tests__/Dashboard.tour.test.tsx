// @vitest-environment jsdom
//
// Render-time half of the tour drift guard: the static guard proves the anchor
// constant is wired in source, but not that it actually paints under a given
// state. The dashboard new-session anchor is conditionally rendered (hidden in
// read-only mode), so assert it resolves exactly once when writable and is gone
// when read-only, matching the step's `writableOnly` metadata.
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

import { Dashboard } from "../Dashboard";
import { TOUR_ANCHORS, tourSelector } from "../../lib/tourSteps";

afterEach(() => {
  cleanup();
});

function renderDashboard(readOnly: boolean) {
  return render(
    <Dashboard
      sessions={[]}
      onSelectSession={vi.fn()}
      onNewSession={vi.fn()}
      onCloneFromUrl={vi.fn()}
      onToggleSidebar={vi.fn()}
      readOnly={readOnly}
    />,
  );
}

describe("Dashboard tour anchors", () => {
  it("renders the new-session anchor exactly once when writable", () => {
    const { container } = renderDashboard(false);
    expect(
      container.querySelectorAll(tourSelector(TOUR_ANCHORS.dashboardNewSession)),
    ).toHaveLength(1);
  });

  it("hides the new-session anchor in read-only mode", () => {
    const { container } = renderDashboard(true);
    expect(
      container.querySelectorAll(tourSelector(TOUR_ANCHORS.dashboardNewSession)),
    ).toHaveLength(0);
  });
});
