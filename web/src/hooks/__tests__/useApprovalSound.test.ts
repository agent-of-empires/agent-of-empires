// @vitest-environment jsdom

// #2146: the structured-view chime is driven by a single count so it can
// cover both approvals and questions. These guard the "attention edge"
// contract the combined-count wiring relies on: a 0 -> >=1 edge chimes
// once (after the replay-quiet window), and a >=1 -> >=2 change does not
// re-chime. A question arriving while an approval is already pending is
// therefore silent on the chime channel, which is why its OS push fires
// unconditionally on the live event edge instead.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

vi.mock("../../lib/api", () => ({
  fetchSettings: vi.fn(async () => ({
    sound: { enabled: true, volume: 1, on_approval: "approval" },
  })),
  fetchSounds: vi.fn(async () => ["approval"]),
  fetchSoundBlob: vi.fn(async () => new Blob(["x"])),
}));

import { useApprovalSound, clearApprovalSoundCache } from "../useApprovalSound";

let audioCount = 0;

beforeEach(() => {
  audioCount = 0;
  clearApprovalSoundCache();
  vi.useFakeTimers();
  // jsdom has no Audio / object-URL plumbing; stub the minimum the hook
  // touches so a successful play() path can be observed by counting
  // constructions.
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
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("useApprovalSound", () => {
  it("chimes once on a 0 -> >=1 edge after the replay-quiet window", async () => {
    const { rerender } = renderHook(({ n }) => useApprovalSound(n), {
      initialProps: { n: 0 },
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1500);
    });
    rerender({ n: 1 });
    await act(async () => {
      await vi.runAllTimersAsync();
    });
    expect(audioCount).toBe(1);
  });

  it("does not re-chime on a >=1 -> >=2 change (no fresh attention edge)", async () => {
    const { rerender } = renderHook(({ n }) => useApprovalSound(n), {
      initialProps: { n: 1 },
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1500);
    });
    rerender({ n: 2 });
    await act(async () => {
      await vi.runAllTimersAsync();
    });
    expect(audioCount).toBe(0);
  });
});
