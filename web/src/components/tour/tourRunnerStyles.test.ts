import { expect, it } from "vitest";
import { TOUR_RUNNER_STYLES } from "./tourRunnerStyles";

// The Skip label must stay on the pairing resolved.rs guarantees AA for every builtin theme.
it("draws Skip tour in text-subtle on the surface-800 tooltip", () => {
  expect(TOUR_RUNNER_STYLES.tooltip?.backgroundColor).toBe("var(--color-surface-800)");
  expect(TOUR_RUNNER_STYLES.buttonSkip?.color).toBe("var(--color-text-subtle)");
});
