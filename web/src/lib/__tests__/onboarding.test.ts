import { describe, expect, it } from "vitest";
import { shouldShowWelcome } from "../onboarding";

const base = {
  autoLaunchReady: true,
  scope: "dashboard" as const,
  readOnly: false,
  automated: false,
  tourSeen: false,
  welcomeSeen: false,
};

describe("shouldShowWelcome", () => {
  it("shows on a settled, writable, never-onboarded dashboard", () => {
    expect(shouldShowWelcome(base)).toBe(true);
  });

  it("waits until the dashboard is settled", () => {
    expect(shouldShowWelcome({ ...base, autoLaunchReady: false })).toBe(false);
  });

  it("only shows on the dashboard scope", () => {
    expect(shouldShowWelcome({ ...base, scope: "session" })).toBe(false);
    expect(shouldShowWelcome({ ...base, scope: "structured-view" })).toBe(false);
  });

  it("is suppressed in read-only mode (cannot persist a theme)", () => {
    expect(shouldShowWelcome({ ...base, readOnly: true })).toBe(false);
  });

  it("is suppressed in automated sessions", () => {
    expect(shouldShowWelcome({ ...base, automated: true })).toBe(false);
  });

  it("does not re-prompt users who already finished the tour (upgraders)", () => {
    expect(shouldShowWelcome({ ...base, tourSeen: true })).toBe(false);
  });

  it("does not show once the welcome has been seen", () => {
    expect(shouldShowWelcome({ ...base, welcomeSeen: true })).toBe(false);
  });

  it("ignores pointer type: shows on touch (unlike the tour auto-launch)", () => {
    // No isDesktop input by design; the predicate has no pointer clause.
    expect(shouldShowWelcome(base)).toBe(true);
  });
});
