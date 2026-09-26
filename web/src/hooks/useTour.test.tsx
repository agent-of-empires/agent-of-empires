// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, renderHook, waitFor } from "@testing-library/react";

import { resolveTourSteps, type TourStep } from "../lib/tourSteps";
import { isAutomatedSession } from "../lib/onboarding";
import { shouldAutoLaunch, useTour, type UseTourOptions } from "./useTour";

vi.mock("../lib/tourSteps", () => ({ resolveTourSteps: vi.fn() }));
vi.mock("../lib/onboarding", () => ({ isAutomatedSession: vi.fn(() => false) }));

let lastOnFinish: ((markSeen: boolean) => void) | null = null;
vi.mock("../components/tour/TourRunner", () => ({
  default: ({ onFinish }: { onFinish: (m: boolean) => void }) => {
    lastOnFinish = onFinish;
    return <div data-testid="tour-runner" />;
  },
}));

const resolveTourStepsMock = vi.mocked(resolveTourSteps);
const isAutomatedSessionMock = vi.mocked(isAutomatedSession);
const STEP = { id: "topbar", anchor: "topbar", scopes: ["dashboard"], title: "t", body: "b" } as TourStep;

let rafQueue: FrameRequestCallback[] = [];
const drainRaf = () =>
  act(() => {
    const q = rafQueue;
    rafQueue = [];
    q.forEach((cb) => cb(performance.now()));
  });

const opts = (over: Partial<UseTourOptions> = {}): UseTourOptions => ({
  scope: "dashboard",
  readOnly: false,
  cityhall: false,
  isDesktop: true,
  autoLaunchReady: true,
  seen: false,
  seenKnown: true,
  onSeen: vi.fn(),
  onNavigate: vi.fn(),
  ...over,
});

beforeEach(() => {
  rafQueue = [];
  lastOnFinish = null;
  vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => rafQueue.push(cb));
  vi.spyOn(window, "cancelAnimationFrame").mockImplementation(() => {});
  isAutomatedSessionMock.mockReturnValue(false);
  resolveTourStepsMock.mockReturnValue([STEP]);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.clearAllMocks();
});

describe("shouldAutoLaunch", () => {
  const base = {
    autoLaunchReady: true,
    seenKnown: true,
    scope: "dashboard" as const,
    isDesktop: true,
    seen: false,
    automated: false,
  };

  it("launches on a settled, unseen dashboard with a fine pointer", () => {
    expect(shouldAutoLaunch(base)).toBe(true);
  });

  it.each<[string, Partial<Parameters<typeof shouldAutoLaunch>[0]>]>([
    ["seen state unknown", { seenKnown: false }],
    ["dashboard not ready", { autoLaunchReady: false }],
    ["session scope", { scope: "session" }],
    ["coarse pointer", { isDesktop: false }],
    ["already seen", { seen: true }],
    ["automated session", { automated: true }],
  ])("does not launch with %s", (_label, over) => {
    expect(shouldAutoLaunch({ ...base, ...over })).toBe(false);
  });
});

describe("useTour", () => {
  it("starts inactive; startTour activates on the next frame with the scope's steps", () => {
    const { result } = renderHook(() => useTour(opts({ autoLaunchReady: false })));
    expect(result.current).toMatchObject({ isTourActive: false, tourElement: null });

    act(() => result.current.startTour());
    expect(result.current.isTourActive).toBe(false);
    drainRaf();
    expect(result.current.isTourActive).toBe(true);
    expect(resolveTourStepsMock).toHaveBeenCalledWith({
      scope: "dashboard",
      readOnly: false,
      cityhall: false,
      isDesktop: true,
    });
  });

  it("startTour is a no-op when no steps resolve", () => {
    resolveTourStepsMock.mockReturnValue([]);
    const { result } = renderHook(() => useTour(opts({ autoLaunchReady: false })));
    act(() => result.current.startTour());
    drainRaf();
    expect(result.current.isTourActive).toBe(false);
  });

  it.each<[string, Partial<UseTourOptions>, boolean, boolean]>([
    ["does not auto-launch inside an automated session", {}, true, false],
  ])("%s", async (_label, over, automated, active) => {
    isAutomatedSessionMock.mockReturnValue(automated);
    const { result } = renderHook(() => useTour(opts(over)));
    drainRaf();
    await waitFor(() => expect(result.current.isTourActive).toBe(active));
  });

  it("auto-launches only once per mount", () => {
    const { result, rerender } = renderHook((p: UseTourOptions) => useTour(p), { initialProps: opts() });
    drainRaf();
    expect(result.current.isTourActive).toBe(true);
    expect(resolveTourStepsMock).toHaveBeenCalledTimes(1);
    rerender(opts());
    drainRaf();
    expect(resolveTourStepsMock).toHaveBeenCalledTimes(1);
  });

  it("cancels an active tour when the scope changes", () => {
    const { result, rerender } = renderHook((p: UseTourOptions) => useTour(p), {
      initialProps: opts({ autoLaunchReady: false }),
    });
    act(() => result.current.startTour());
    drainRaf();
    expect(result.current.isTourActive).toBe(true);
    act(() => rerender(opts({ autoLaunchReady: false, scope: "session" })));
    expect(result.current.isTourActive).toBe(false);
  });

  it.each([true, false])("finishing with markSeen=%s stops the tour and persists only when true", async (markSeen) => {
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
    act(() => getByText("start").click());
    drainRaf();
    await waitFor(() => expect(queryByTestId("tour-runner")).not.toBeNull());
    act(() => lastOnFinish?.(markSeen));
    expect(onSeen).toHaveBeenCalledTimes(markSeen ? 1 : 0);
    await waitFor(() => expect(queryByTestId("tour-runner")).toBeNull());
  });
});
