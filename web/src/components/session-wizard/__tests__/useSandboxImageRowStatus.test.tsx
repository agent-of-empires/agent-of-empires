// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook } from "@testing-library/react";

import { fetchSandboxImageStatus, type SandboxImageStatus } from "../../../lib/api";
import { IMAGE_RECHECK_MS, sandboxImageStatusLabel, useSandboxImageRowStatus } from "../useSandboxImageRowStatus";

vi.mock("../../../lib/api", () => ({ fetchSandboxImageStatus: vi.fn() }));
const fetchStatus = vi.mocked(fetchSandboxImageStatus);

const status = (state: Record<string, unknown>) => ({ image: "sb:1", ...state }) as unknown as SandboxImageStatus;

describe("sandboxImageStatusLabel", () => {
  it.each([
    ["an update", status({ state: "available", remote_digest: "sha256:b" }), "Update available"],
    ["the latest image", status({ state: "current" }), "Up to date"],
    ["an image not pulled yet", status({ state: "unknown", reason: "not_pulled" }), "Downloads on launch"],
    ["a pinned image", status({ state: "unknown", reason: "pinned" }), "Update status unknown"],
    ["a failed registry check", status({ state: "check_failed", error: "timeout" }), "Couldn't check for updates"],
    ["an image that isn't configured", { error: "unknown_image" }, "Update status unknown"],
    ["no answer", null, "Update status unknown"],
  ])("labels %s", (_name, reply, label) => {
    expect(sandboxImageStatusLabel(reply)).toBe(label);
  });
});

describe("useSandboxImageRowStatus", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    fetchStatus.mockReset().mockResolvedValue(status({ state: "current" }));
  });
  afterEach(() => {
    cleanup();
    vi.useRealTimers();
  });

  const settle = () => act(async () => {});

  it("asks fresh on open, then re-checks an edited image once typing stops, from the cache", async () => {
    const { result, rerender } = renderHook(({ image }) => useSandboxImageRowStatus(image, true), {
      initialProps: { image: "sb:1" },
    });
    await settle();
    expect(fetchStatus.mock.calls).toEqual([["sb:1", { fresh: true }]]);
    expect(result.current).toBe("Up to date");

    fetchStatus.mockResolvedValue({ error: "unknown_image" });
    rerender({ image: "sb:2" });
    rerender({ image: "sb:22" });
    // The old image's label doesn't stand for the new one.
    expect(result.current).toBeNull();
    await act(async () => vi.advanceTimersByTime(IMAGE_RECHECK_MS - 1));
    expect(fetchStatus).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTime(1));
    expect(fetchStatus.mock.calls.slice(1)).toEqual([["sb:22", { fresh: false }]]);
    expect(result.current).toBe("Update status unknown");
  });

  it("asks for the default image when none is set, and not at all while disabled", async () => {
    const { result, rerender } = renderHook(({ enabled }) => useSandboxImageRowStatus("", enabled), {
      initialProps: { enabled: false },
    });
    await settle();
    expect(fetchStatus).not.toHaveBeenCalled();
    rerender({ enabled: true });
    await settle();
    expect(fetchStatus.mock.calls).toEqual([[undefined, { fresh: true }]]);
    expect(result.current).toBe("Up to date");
  });

  it("drops a late answer for a previous image", async () => {
    let answerFirst: (s: SandboxImageStatus) => void = () => {};
    fetchStatus.mockReturnValueOnce(new Promise((resolve) => (answerFirst = resolve)));
    const { result, rerender } = renderHook(({ image }) => useSandboxImageRowStatus(image, true), {
      initialProps: { image: "sb:1" },
    });
    rerender({ image: "sb:2" });
    await settle();
    expect(result.current).toBe("Up to date");
    // The first image's answer lands last and changes nothing.
    await act(async () => answerFirst(status({ state: "available", remote_digest: "sha256:b" })));
    expect(result.current).toBe("Up to date");
    // Nothing was answered yet, so the edited image is still the opening check.
    expect(fetchStatus.mock.calls.at(-1)).toEqual(["sb:2", { fresh: true }]);
  });
});
