import { describe, expect, it } from "vitest";
import { TOUR_RUNNER_STYLES } from "./tourRunnerStyles";

describe("TourRunner theme styles", () => {
  it("uses the resolved text-on-brand token for the primary button", () => {
    expect(TOUR_RUNNER_STYLES.buttonPrimary?.color).toBe("var(--color-text-on-brand)");
  });
});
