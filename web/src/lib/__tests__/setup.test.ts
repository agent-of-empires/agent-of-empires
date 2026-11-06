import { describe, expect, it } from "vitest";
import type { SetupStatus, SetupStep, SetupStepKey } from "../api";
import { bannerSteps, firstOpenIndex, openRequiredStep, shouldRedirectToSetup } from "../setup";

const KEYS: SetupStepKey[] = ["theme_telemetry", "agents", "project", "worktrees", "sandbox", "notifications"];

function status(over: Partial<SetupStatus> = {}, steps: Partial<Record<SetupStepKey, Partial<SetupStep>>> = {}) {
  return {
    finished: false,
    deferred: false,
    banner_dismissed: false,
    tmux: true,
    git: true,
    access: { serve_mode: "local" },
    steps: KEYS.map((key) => ({ key, required: key === "agents", configured: false, skipped: false, ...steps[key] })),
    ...over,
  } satisfies SetupStatus;
}

const viewer = { readOnly: false, cityhall: false, automated: false };

describe("shouldRedirectToSetup", () => {
  it("redirects only an unfinished, undeferred, writable dashboard root", () => {
    expect(shouldRedirectToSetup({ ...viewer, status: status(), pathname: "/" })).toBe(true);
    for (const [label, over] of [
      ["deep link", { pathname: "/settings/theme" }],
      ["finished", { status: status({ finished: true }) }],
      ["deferred", { status: status({ deferred: true }) }],
      ["read-only", { readOnly: true }],
      ["CityHall", { cityhall: true }],
      ["automated", { automated: true }],
      ["status unavailable", { status: null }],
    ] as const) {
      expect(shouldRedirectToSetup({ ...viewer, status: status(), pathname: "/", ...over }), label).toBe(false);
    }
  });
});

describe("bannerSteps", () => {
  it("lists every unconfigured step, skipped ones included, until dismissed", () => {
    const s = status({ finished: true }, { agents: { configured: true }, sandbox: { skipped: true } });
    expect(bannerSteps({ ...viewer, status: s }).map((step) => step.key)).toEqual([
      "theme_telemetry",
      "project",
      "worktrees",
      "sandbox",
      "notifications",
    ]);
    expect(bannerSteps({ ...viewer, status: { ...s, banner_dismissed: true } })).toEqual([]);
    expect(bannerSteps({ ...viewer, readOnly: true, status: s })).toEqual([]);
    expect(bannerSteps({ ...viewer, cityhall: true, status: s })).toEqual([]);
  });
});

describe("step progress", () => {
  it("opens on the first step that is neither configured nor skipped", () => {
    expect(
      firstOpenIndex(status({}, { theme_telemetry: { configured: true }, agents: { configured: true } }).steps),
    ).toBe(2);
    expect(firstOpenIndex(status({}, { theme_telemetry: { skipped: true } }).steps)).toBe(1);
    const all = Object.fromEntries(KEYS.map((k) => [k, { configured: true }]));
    expect(firstOpenIndex(status({}, all).steps)).toBe(0);
  });

  it("blocks finish on an unconfigured required step", () => {
    expect(openRequiredStep(status().steps)?.key).toBe("agents");
    expect(openRequiredStep(status({}, { agents: { configured: true } }).steps)).toBeUndefined();
  });
});
