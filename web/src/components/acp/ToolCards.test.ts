import { describe, expect, it } from "vitest";
import { formatDurationMs } from "./ToolCards";

describe("formatDurationMs", () => {
  it("renders sub-second durations as ms", () => {
    expect(formatDurationMs(0)).toBe("0 ms");
    expect(formatDurationMs(45)).toBe("45 ms");
    expect(formatDurationMs(999)).toBe("999 ms");
  });

  it("renders seconds with one decimal", () => {
    expect(formatDurationMs(1000)).toBe("1.0s");
    expect(formatDurationMs(4231)).toBe("4.2s");
    expect(formatDurationMs(59_999)).toBe("60.0s");
  });

  it("renders ≥ 1 minute as m s", () => {
    expect(formatDurationMs(60_000)).toBe("1m 0s");
    expect(formatDurationMs(72_400)).toBe("1m 12s");
    expect(formatDurationMs(3_600_000)).toBe("60m 0s");
  });
});
