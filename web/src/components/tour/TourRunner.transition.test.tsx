// @vitest-environment jsdom
//
// TourRunner drives react-joyride in controlled mode so the settings-modal
// steps (whose anchors live behind a route transition) never hand the engine a
// missing target. These tests mock react-joyride down to a prop sink so we can
// assert the transition contract directly: navigate on the crossing, suspend
// (unmount) while the DOM mutates, poll for the anchor, remount at the new
// index. See TourRunner.tsx and issue #2633.
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, waitFor } from "@testing-library/react";
import TourRunner from "./TourRunner";
import { TOUR_RUNNER_OPTIONS } from "./tourRunnerStyles";
import { TOUR_ANCHORS, type TourStep } from "../../lib/tourSteps";

vi.mock("react-joyride", async () => {
  const { createElement } = await import("react");
  let latestOnEvent: ((data: unknown) => void) | null = null;
  const Joyride = (props: { stepIndex: number; run: boolean; onEvent: (data: unknown) => void }) => {
    latestOnEvent = props.onEvent;
    return createElement("div", {
      "data-testid": "joyride",
      "data-step-index": props.stepIndex,
      "data-run": String(props.run),
    });
  };
  return {
    Joyride,
    EVENTS: { STEP_AFTER: "step:after", TOUR_END: "tour:end", TARGET_NOT_FOUND: "error:target_not_found" },
    ACTIONS: { PREV: "prev", NEXT: "next", CLOSE: "close", SKIP: "skip" },
    STATUS: { FINISHED: "finished", SKIPPED: "skipped" },
    __getOnEvent: () => latestOnEvent,
  };
});

import * as joyrideMock from "react-joyride";

const fire = (data: Record<string, unknown>) => {
  const onEvent = (joyrideMock as unknown as { __getOnEvent: () => (d: unknown) => void }).__getOnEvent();
  act(() => onEvent(data));
};

function addAnchor(value: string) {
  const el = document.createElement("div");
  el.setAttribute("data-tour", value);
  document.body.appendChild(el);
  return el;
}

const dashStep: TourStep = {
  id: "topbar",
  anchor: TOUR_ANCHORS.topbar,
  scopes: ["dashboard"],
  title: "t",
  body: "b",
};
const dashStep2: TourStep = {
  id: "new-session",
  anchor: TOUR_ANCHORS.dashboardNewSession,
  scopes: ["dashboard"],
  title: "t2",
  body: "b2",
};
const worktreeStep: TourStep = {
  id: "settings-worktree",
  anchor: TOUR_ANCHORS.settingsWorktree,
  settingsTab: "worktree",
  scopes: ["dashboard"],
  title: "wt",
  body: "wt body",
};

afterEach(() => {
  cleanup();
  document.body.innerHTML = "";
});

describe("TourRunner controlled transitions", () => {
  it("navigates, suspends, and remounts at the new index when crossing into a settings step", async () => {
    addAnchor(TOUR_ANCHORS.topbar);
    const onNavigate = vi.fn();
    const onFinish = vi.fn();
    const { queryByTestId, getByTestId } = render(
      <TourRunner run steps={[dashStep, worktreeStep]} onFinish={onFinish} onNavigate={onNavigate} />,
    );
    expect(getByTestId("joyride").getAttribute("data-step-index")).toBe("0");

    // Advance from the dashboard step into the worktree settings step.
    fire({ type: "step:after", index: 0, action: "next", status: "" });

    // Host is told to open the worktree tab, and Joyride unmounts (suspended)
    // so it cannot fault on the not-yet-painted anchor.
    expect(onNavigate).toHaveBeenCalledWith("worktree");
    expect(queryByTestId("joyride")).toBeNull();

    // The tab paints its anchor; the poll lifts the suspension and remounts at 1.
    addAnchor(TOUR_ANCHORS.settingsWorktree);
    await waitFor(() => expect(queryByTestId("joyride")).not.toBeNull());
    expect(getByTestId("joyride").getAttribute("data-step-index")).toBe("1");
  });

  it("does not navigate or suspend between two dashboard steps", () => {
    addAnchor(TOUR_ANCHORS.topbar);
    addAnchor(TOUR_ANCHORS.dashboardNewSession);
    const onNavigate = vi.fn();
    const { getByTestId } = render(
      <TourRunner run steps={[dashStep, dashStep2]} onFinish={vi.fn()} onNavigate={onNavigate} />,
    );
    fire({ type: "step:after", index: 0, action: "next", status: "" });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(getByTestId("joyride").getAttribute("data-step-index")).toBe("1");
  });

  it("closes settings on Back out of a settings step and resumes at the prior index", async () => {
    addAnchor(TOUR_ANCHORS.topbar);
    const onNavigate = vi.fn();
    const { queryByTestId, getByTestId } = render(
      <TourRunner run steps={[dashStep, worktreeStep]} onFinish={vi.fn()} onNavigate={onNavigate} />,
    );
    // Forward into the worktree settings step.
    fire({ type: "step:after", index: 0, action: "next", status: "" });
    addAnchor(TOUR_ANCHORS.settingsWorktree);
    await waitFor(() => expect(getByTestId("joyride").getAttribute("data-step-index")).toBe("1"));

    // Back out: host is told to close Settings, tour suspends, then remounts at 0.
    fire({ type: "step:after", index: 1, action: "prev", status: "" });
    expect(onNavigate).toHaveBeenLastCalledWith(null);
    expect(queryByTestId("joyride")).toBeNull();
    await waitFor(() => expect(queryByTestId("joyride")).not.toBeNull());
    expect(getByTestId("joyride").getAttribute("data-step-index")).toBe("0");
  });

  it("closes settings and marks seen when the tour ends on a settings step", () => {
    addAnchor(TOUR_ANCHORS.settingsWorktree);
    const onNavigate = vi.fn();
    const onFinish = vi.fn();
    render(<TourRunner run steps={[worktreeStep]} onFinish={onFinish} onNavigate={onNavigate} />);
    fire({ type: "tour:end", index: 0, action: "skip", status: "skipped" });
    expect(onNavigate).toHaveBeenCalledWith(null);
    expect(onFinish).toHaveBeenCalledWith(true);
  });

  // #2819: dismissing the tour (Escape, or clicking the dim overlay) reaches us
  // as a STEP_AFTER with action=CLOSE while status is still RUNNING, because
  // react-joyride's controlled mode does not flip status to FINISHED on a
  // non-last close. It must end the tour, not fall through to the +1 advance.
  it("ends and marks seen on a close dismiss instead of advancing", () => {
    addAnchor(TOUR_ANCHORS.topbar);
    addAnchor(TOUR_ANCHORS.dashboardNewSession);
    const onFinish = vi.fn();
    const { getByTestId } = render(
      <TourRunner run steps={[dashStep, dashStep2]} onFinish={onFinish} onNavigate={vi.fn()} />,
    );
    fire({ type: "step:after", index: 0, action: "close", status: "" });
    expect(onFinish).toHaveBeenCalledWith(true);
    // Never advanced: still parked on step 0.
    expect(getByTestId("joyride").getAttribute("data-step-index")).toBe("0");
  });

  // #2819: advancing past the last step is the user finishing (Done). Each
  // settings crossing remounts Joyride, so the engine emits no TOUR_END on the
  // last step in that flow; the handler must end on the past-last advance
  // itself, or the overlay strands.
  it("ends and marks seen when advancing past the last step", () => {
    addAnchor(TOUR_ANCHORS.topbar);
    addAnchor(TOUR_ANCHORS.dashboardNewSession);
    const onFinish = vi.fn();
    render(<TourRunner run steps={[dashStep, dashStep2]} onFinish={onFinish} onNavigate={vi.fn()} />);
    fire({ type: "step:after", index: 1, action: "next", status: "" });
    expect(onFinish).toHaveBeenCalledWith(true);
  });

  it("does not advance on a non-navigation action", () => {
    addAnchor(TOUR_ANCHORS.topbar);
    addAnchor(TOUR_ANCHORS.dashboardNewSession);
    const onFinish = vi.fn();
    const { getByTestId } = render(
      <TourRunner run steps={[dashStep, dashStep2]} onFinish={onFinish} onNavigate={vi.fn()} />,
    );
    fire({ type: "step:after", index: 0, action: "update", status: "" });
    expect(onFinish).not.toHaveBeenCalled();
    expect(getByTestId("joyride").getAttribute("data-step-index")).toBe("0");
  });

  it("ends without marking seen when the target is missing", () => {
    addAnchor(TOUR_ANCHORS.topbar);
    const onFinish = vi.fn();
    render(<TourRunner run steps={[dashStep, dashStep2]} onFinish={onFinish} onNavigate={vi.fn()} />);
    fire({ type: "error:target_not_found", index: 1, action: "next", status: "" });
    expect(onFinish).toHaveBeenCalledWith(false);
  });

  it("disables react-joyride's overlay-click dismiss (the strand-prone path)", () => {
    expect(TOUR_RUNNER_OPTIONS.overlayClickAction).toBe(false);
  });
});
