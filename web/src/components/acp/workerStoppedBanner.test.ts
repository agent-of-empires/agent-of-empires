import { describe, expect, it } from "vitest";
import { pickWorkerStoppedVariant, showWorkerStoppingBanner } from "./workerStoppedBanner";

const T = "2026-01-01T00:00:00Z";
const base = { workerStopped: true, startupError: null, trashedAt: null, archivedAt: null, snoozedUntil: null };

describe("workerStoppedBanner", () => {
  it("pickWorkerStoppedVariant", () => {
    const cases: [string, Partial<Parameters<typeof pickWorkerStoppedVariant>[0]>, string][] = [
      ["not stopped", { workerStopped: false }, "none"],
      ["startup error owns the chrome", { startupError: "missing API key" }, "none"],
      ["trashed", { trashedAt: T }, "trashed"],
      ["trash wins over archive and snooze", { trashedAt: T, archivedAt: T, snoozedUntil: T }, "trashed"],
      // Reconnecting an archived or snoozed session would race the reconciler, which skips it.
      ["archived", { archivedAt: T }, "archived"],
      ["snoozed", { snoozedUntil: T }, "snoozed"],
      ["archive wins over snooze", { archivedAt: T, snoozedUntil: T }, "archived"],
      ["external teardown", {}, "generic"],
      ["empty startup error is not an error", { startupError: "" }, "generic"],
      ["empty startup error falls through to archived", { startupError: "", archivedAt: T }, "archived"],
      ["stopping yields the generic banner", { workerStopping: true }, "none"],
      ["not stopping keeps the generic banner", { workerStopping: false }, "generic"],
      ["stopping keeps triage banners", { workerStopping: true, archivedAt: T }, "archived"],
    ];
    for (const [label, overrides, expected] of cases) {
      expect(pickWorkerStoppedVariant({ ...base, ...overrides }), label).toBe(expected);
    }
  });

  it("showWorkerStoppingBanner", () => {
    const cases: [string, string | null, boolean][] = [
      ["stopping", null, true],
      ["stopping", "missing API key", false],
      ["resuming", null, false],
      ["absent", null, false],
    ];
    for (const [acpWorkerState, startupError, expected] of cases) {
      expect(showWorkerStoppingBanner({ acpWorkerState, startupError }), `${acpWorkerState}/${startupError}`).toBe(
        expected,
      );
    }
  });
});
