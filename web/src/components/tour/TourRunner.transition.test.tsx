// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import TourRunner from "./TourRunner";
import { TOUR_RUNNER_OPTIONS, TOUR_RUNNER_STYLES } from "./tourRunnerStyles";
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

afterEach(() => {
  cleanup();
  document.body.innerHTML = "";
});

describe("TourRunner controlled transitions", () => {
  const stepIndex = (api: ReturnType<typeof render>) => api.getByTestId("joyride").getAttribute("data-step-index");
  const renderTour = (steps: TourStep[], anchors: string[]) => {
    anchors.forEach(addAnchor);
    const onFinish = vi.fn();
    const api = render(<TourRunner run steps={steps} onFinish={onFinish} />);
    return { api, onFinish };
  };
  const twoDash = () => renderTour([dashStep, dashStep2], [TOUR_ANCHORS.topbar, TOUR_ANCHORS.dashboardNewSession]);

  it("advances between steps and marks seen when the tour is skipped", () => {
    const { api, onFinish } = twoDash();
    fire({ type: "step:after", index: 0, action: "next", status: "" });
    expect(stepIndex(api)).toBe("1");
    fire({ type: "step:after", index: 1, action: "prev", status: "" });
    expect(stepIndex(api)).toBe("0");
    fire({ type: "tour:end", index: 0, action: "skip", status: "skipped" });
    expect(onFinish).toHaveBeenCalledWith(true);
  });

  // A close dismiss arrives as STEP_AFTER with status still running.
  it.each([
    ["a close dismiss", 0, "close", true, "0"],
    ["advancing past the last step", 1, "next", true, "1"],
    ["a non-navigation action", 0, "update", undefined, "0"],
  ])("handles %s", (_, index, action, markSeen, parked) => {
    const { api, onFinish } = twoDash();
    fire({ type: "step:after", index, action, status: "" });
    if (markSeen === undefined) expect(onFinish).not.toHaveBeenCalled();
    else expect(onFinish).toHaveBeenCalledWith(markSeen);
    if (index === 0) expect(stepIndex(api)).toBe(parked);
  });

  it("ends without marking seen when the target is missing", () => {
    const { onFinish } = renderTour([dashStep, dashStep2], [TOUR_ANCHORS.topbar]);
    fire({ type: "error:target_not_found", index: 1, action: "next", status: "" });
    expect(onFinish).toHaveBeenCalledWith(false);
  });

  it("disables overlay-click dismiss and themes the primary button", () => {
    expect(TOUR_RUNNER_OPTIONS.overlayClickAction).toBe(false);
    expect(TOUR_RUNNER_STYLES.buttonPrimary?.color).toBe("var(--color-text-on-brand)");
  });
});
