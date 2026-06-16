// @vitest-environment jsdom

// #2146: the structured-view chime covers both approvals and questions via a
// single combined count. AttentionChime is the small extracted wrapper (the
// #1282 pattern) so this wiring is unit-mountable without the assistant-ui
// runtime. These guard the "attention edge" contract: a 0 -> >=1 edge chimes
// once (after the replay-quiet window), and a >=1 -> >=2 change does not
// re-chime. A question arriving while an approval is already pending is
// therefore silent on the chime channel, which is why its OS push fires
// unconditionally on the live event edge instead.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

vi.mock("../../../lib/api", () => ({
  fetchSettings: vi.fn(async () => ({
    sound: { enabled: true, volume: 1, on_approval: "approval" },
  })),
  fetchSounds: vi.fn(async () => ["approval"]),
  fetchSoundBlob: vi.fn(async () => new Blob(["x"])),
}));

import { AttentionChime } from "../AttentionChime";
import { clearApprovalSoundCache } from "../../../hooks/useApprovalSound";

let audioCount = 0;

beforeEach(() => {
  audioCount = 0;
  clearApprovalSoundCache();
  vi.useFakeTimers();
  // jsdom has no Audio / object-URL plumbing; stub the minimum the hook
  // touches so a successful play() path is observable by counting builds.
  vi.stubGlobal(
    "Audio",
    class {
      volume = 1;
      constructor(_src: string) {
        audioCount++;
      }
      play() {
        return Promise.resolve();
      }
    },
  );
  vi.stubGlobal("URL", {
    ...URL,
    createObjectURL: vi.fn(() => "blob:stub"),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("AttentionChime", () => {
  it("chimes once when a question arrives (0 -> >=1 combined edge)", async () => {
    const { rerender } = render(<AttentionChime approvals={0} elicitations={0} />);
    await vi.advanceTimersByTimeAsync(1500);
    rerender(<AttentionChime approvals={0} elicitations={1} />);
    await vi.runAllTimersAsync();
    expect(audioCount).toBe(1);
  });

  it("does not re-chime when a second item arrives (>=1 -> >=2)", async () => {
    const { rerender } = render(<AttentionChime approvals={1} elicitations={0} />);
    await vi.advanceTimersByTimeAsync(1500);
    rerender(<AttentionChime approvals={1} elicitations={1} />);
    await vi.runAllTimersAsync();
    expect(audioCount).toBe(0);
  });
});
