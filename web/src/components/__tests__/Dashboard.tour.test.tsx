// @vitest-environment jsdom
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
    expect(container.querySelectorAll(tourSelector(TOUR_ANCHORS.dashboardNewSession))).toHaveLength(1);
  });

  it("hides the new-session anchor in read-only mode", () => {
    const { container } = renderDashboard(true);
    expect(container.querySelectorAll(tourSelector(TOUR_ANCHORS.dashboardNewSession))).toHaveLength(0);
  });
});
