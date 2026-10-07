// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

import { Dashboard } from "../Dashboard";
import { TOUR_ANCHORS, tourSelector } from "../../lib/tourSteps";

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
  it("renders the new-session anchor once when writable and hides it in read-only mode", () => {
    const anchor = tourSelector(TOUR_ANCHORS.dashboardNewSession);
    expect(renderDashboard(false).container.querySelectorAll(anchor)).toHaveLength(1);
    cleanup();
    expect(renderDashboard(true).container.querySelectorAll(anchor)).toHaveLength(0);
  });
});
