// Auto-launch decision table for the first-run tutorial. The run -> engine
// integration (rAF, lazy load, Joyride rendering, skip persistence) is covered
// by the live Playwright smoke; this locks the pure gating logic that live
// (desktop, first-run) cannot easily exercise: coarse-pointer and seen-flag
// suppression, and scope gating.
import { describe, expect, it } from "vitest";
import { shouldAutoLaunch } from "../useTour";

const base = {
  autoLaunchReady: true,
  seenKnown: true,
  scope: "dashboard" as const,
  isDesktop: true,
  seen: false,
  automated: false,
};

describe("shouldAutoLaunch", () => {
  it("launches on a settled dashboard, fine pointer, unseen", () => {
    expect(shouldAutoLaunch(base)).toBe(true);
  });

  it("does not launch before the seen state is known", () => {
    expect(shouldAutoLaunch({ ...base, seenKnown: false })).toBe(false);
  });

  it("does not launch before the dashboard is ready", () => {
    expect(shouldAutoLaunch({ ...base, autoLaunchReady: false })).toBe(false);
  });

  it("does not launch outside the dashboard scope", () => {
    expect(shouldAutoLaunch({ ...base, scope: "session" })).toBe(false);
    expect(shouldAutoLaunch({ ...base, scope: "structured-view" })).toBe(false);
  });

  it("does not auto-launch on coarse pointers", () => {
    expect(shouldAutoLaunch({ ...base, isDesktop: false })).toBe(false);
  });

  it("does not auto-launch once the tour has been seen", () => {
    expect(shouldAutoLaunch({ ...base, seen: true })).toBe(false);
  });

  it("does not auto-launch inside an automated browser session", () => {
    expect(shouldAutoLaunch({ ...base, automated: true })).toBe(false);
  });
});
