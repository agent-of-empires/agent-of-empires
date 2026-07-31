// @vitest-environment jsdom

// Drives the useTour hook itself (run-state machine, auto-launch effect,
// scope-change cancel, finish-with-markSeen persistence). The pure
// shouldAutoLaunch truth table is covered by ./__tests__/useTour.test.ts; this
// file does NOT duplicate it.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, renderHook, waitFor } from "@testing-library/react";

import { resolveTourSteps, type TourStep } from "../lib/tourSteps";
import { isAutomatedSession } from "../lib/onboarding";
import { useTour, type UseTourOptions } from "./useTour";

vi.mock("../lib/tourSteps", () => ({
  resolveTourSteps: vi.fn(),
}));

vi.mock("../lib/onboarding", () => ({
  isAutomatedSession: vi.fn(() => false),
}));

// Stub the lazy engine so tourElement renders deterministically and exposes an
// onFinish handle, without pulling in react-joyride.
let lastOnFinish: ((markSeen: boolean) => void) | null = null;
vi.mock("../components/tour/TourRunner", () => ({
  default: ({ run, onFinish }: { run: boolean; onFinish: (m: boolean) => void }) => {
    lastOnFinish = onFinish;
    return <div data-testid="tour-runner" data-run={String(run)} />;
  },
}));

const resolveTourStepsMock = vi.mocked(resolveTourSteps);
const isAutomatedSessionMock = vi.mocked(isAutomatedSession);

const STEP: TourStep = {
  id: "topbar",
  anchor: "topbar" as TourStep["anchor"],
  scopes: ["dashboard"],
  title: "t",
  body: "b",
};

let rafQueue: FrameRequestCallback[] = [];

function drainRaf() {
  const q = rafQueue;
  rafQueue = [];
  q.forEach((cb) => cb(performance.now()));
}

function opts(over: Partial<UseTourOptions> = {}): UseTourOptions {
  return {
    scope: "dashboard",
    readOnly: false,
    cityhall: false,
    isDesktop: true,
    autoLaunchReady: true,
    seen: false,
    seenKnown: true,
    onSeen: vi.fn(),
    ...over,
  };
}

beforeEach(() => {
  rafQueue = [];
  lastOnFinish = null;
  vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb: FrameRequestCallback) => {
    rafQueue.push(cb);
    return rafQueue.length;
  });
  vi.spyOn(window, "cancelAnimationFrame").mockImplementation(() => {});
  isAutomatedSessionMock.mockReturnValue(false);
  resolveTourStepsMock.mockReturnValue([STEP]);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.clearAllMocks();
});

describe("useTour hook behavior", () => {
  it("starts inactive and exposes a null tourElement", () => {
    const { result } = renderHook(() => useTour(opts()));
    expect(result.current.isTourActive).toBe(false);
    expect(result.current.tourElement).toBeNull();
  });

  it("startTour resolves steps on the next frame and activates", () => {
    const { result } = renderHook(() => useTour(opts({ autoLaunchReady: false })));

    act(() => {
      result.current.startTour();
    });
    expect(result.current.isTourActive).toBe(false); // not until the frame fires

    act(() => drainRaf());
    expect(result.current.isTourActive).toBe(true);
    expect(resolveTourStepsMock).toHaveBeenCalledWith({
      scope: "dashboard",
      readOnly: false,
      cityhall: false,
      isDesktop: true,
    });
  });

  it("startTour is a no-op when no steps resolve for the scope", () => {
    resolveTourStepsMock.mockReturnValue([]);
    const { result } = renderHook(() => useTour(opts({ autoLaunchReady: false })));

    act(() => {
      result.current.startTour();
    });
    act(() => drainRaf());
    expect(result.current.isTourActive).toBe(false);
  });

  it("auto-launches on a settled, unseen dashboard", async () => {
    const { result } = renderHook(() => useTour(opts()));
    act(() => drainRaf());
    await waitFor(() => expect(result.current.isTourActive).toBe(true));
  });

  it("does not auto-launch inside an automated session", () => {
    isAutomatedSessionMock.mockReturnValue(true);
    const { result } = renderHook(() => useTour(opts()));
    act(() => drainRaf());
    expect(result.current.isTourActive).toBe(false);
  });

  it("does not auto-launch when already seen", () => {
    const { result } = renderHook(() => useTour(opts({ seen: true })));
    act(() => drainRaf());
    expect(result.current.isTourActive).toBe(false);
  });

  it("auto-launches only once per mount", () => {
    const { result, rerender } = renderHook((p: UseTourOptions) => useTour(p), {
      initialProps: opts(),
    });
    act(() => drainRaf());
    expect(result.current.isTourActive).toBe(true);
    expect(resolveTourStepsMock).toHaveBeenCalledTimes(1);

    // Finish, then re-render with the same gating props: no second auto-launch.
    act(() => result.current.startTour()); // ensure a runner is mounted
    rerender(opts({ autoLaunchReady: true }));
    act(() => drainRaf());
    // resolveTourSteps may be called again by the explicit startTour above, but
    // the auto-launch effect must not fire a fresh begin() on its own re-run.
    const callsAfter = resolveTourStepsMock.mock.calls.length;
    rerender(opts({ autoLaunchReady: true }));
    act(() => drainRaf());
    expect(resolveTourStepsMock.mock.calls.length).toBe(callsAfter);
  });

  it("cancels an active tour when the scope changes", () => {
    const { result, rerender } = renderHook((p: UseTourOptions) => useTour(p), {
      initialProps: opts({ autoLaunchReady: false }),
    });
    act(() => result.current.startTour());
    act(() => drainRaf());
    expect(result.current.isTourActive).toBe(true);

    act(() => {
      rerender(opts({ autoLaunchReady: false, scope: "session" }));
    });
    expect(result.current.isTourActive).toBe(false);
  });

  it("finishing with markSeen=true persists via onSeen and stops the tour", async () => {
    const onSeen = vi.fn();

    function Harness() {
      const tour = useTour(opts({ autoLaunchReady: false, onSeen }));
      // Expose startTour through a button so we can render the tourElement too.
      return (
        <div>
          <button onClick={tour.startTour}>start</button>
          {tour.tourElement}
        </div>
      );
    }

    const { getByText, queryByTestId } = render(<Harness />);

    act(() => {
      getByText("start").click();
    });
    act(() => drainRaf());

    await waitFor(() => expect(queryByTestId("tour-runner")).not.toBeNull());

    act(() => {
      lastOnFinish?.(true);
    });

    expect(onSeen).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(queryByTestId("tour-runner")).toBeNull());
  });

  it("finishing with markSeen=false stops the tour without persisting", async () => {
    const onSeen = vi.fn();

    function Harness() {
      const tour = useTour(opts({ autoLaunchReady: false, onSeen }));
      return (
        <div>
          <button onClick={tour.startTour}>start</button>
          {tour.tourElement}
        </div>
      );
    }

    const { getByText, queryByTestId } = render(<Harness />);
    act(() => {
      getByText("start").click();
    });
    act(() => drainRaf());
    await waitFor(() => expect(queryByTestId("tour-runner")).not.toBeNull());

    act(() => {
      lastOnFinish?.(false);
    });

    expect(onSeen).not.toHaveBeenCalled();
    await waitFor(() => expect(queryByTestId("tour-runner")).toBeNull());
  });
});
