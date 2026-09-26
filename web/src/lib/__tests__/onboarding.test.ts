import { expect, it } from "vitest";
import { shouldShowWelcome } from "../onboarding";

const base = {
  autoLaunchReady: true,
  scope: "dashboard" as const,
  readOnly: false,
  automated: false,
  tourSeen: false,
  welcomeSeen: false,
};

it("shouldShowWelcome only on a settled, writable, never-onboarded dashboard", () => {
  expect(shouldShowWelcome(base)).toBe(true);
  const blockers: Partial<Parameters<typeof shouldShowWelcome>[0]>[] = [
    { autoLaunchReady: false },
    { scope: "session" },
    { scope: "structured-view" },
    // Read-only cannot persist a theme.
    { readOnly: true },
    { automated: true },
    // Upgraders who already finished the tour.
    { tourSeen: true },
    { welcomeSeen: true },
  ];
  for (const over of blockers) expect(shouldShowWelcome({ ...base, ...over }), JSON.stringify(over)).toBe(false);
});
