// @vitest-environment jsdom

import { describe, expect, it, beforeEach, afterEach, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

const fetchSettings = vi.fn();
const fetchSound = vi.fn();

vi.mock("../lib/api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchSound: (...args: unknown[]) => fetchSound(...args),
}));

import { useApprovalSound } from "./useApprovalSound";
import { notifySettingsChanged } from "../lib/settingsEvents";
import { approvalChimedRecently, clearSoundCache } from "../lib/soundPlayer";
import { fakeAudio, installFakeAudioContext, playedSounds, soundBytes } from "../lib/__tests__/fakeAudioContext";

const REPLAY_QUIET_MS = 1500;
// Longer than soundPlayer's wait for a context that may never resume.
const RESUME_WAIT_MS = 250;

async function flushPlayback(): Promise<void> {
  await act(async () => {
    vi.advanceTimersByTime(0);
    for (let i = 0; i < 4; i++) await Promise.resolve();
  });
}

describe("useApprovalSound", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    installFakeAudioContext();
    fetchSettings.mockReset().mockResolvedValue({ sound: { enabled: true, browser_volume: 1.0 } });
    fetchSound.mockReset().mockImplementation(async (event: string) => soundBytes(event));
    clearSoundCache();
  });

  afterEach(() => {
    vi.runOnlyPendingTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  /** Mount at 0, wait out the replay grace window, then walk through `counts`. */
  async function walk(...counts: number[]) {
    const view = renderHook((p: number) => useApprovalSound(p), { initialProps: 0 });
    await act(async () => {
      vi.advanceTimersByTime(REPLAY_QUIET_MS);
    });
    for (const count of counts) {
      view.rerender(count);
      await flushPlayback();
    }
    return view;
  }

  it("plays the approval sound once per 0 -> >=1 edge after the grace window", async () => {
    await walk(1, 3);
    expect(playedSounds()).toEqual(["approval"]);
    expect(fetchSound).toHaveBeenCalledWith("approval");

    await walk(1, 0, 1);
    expect(playedSounds()).toHaveLength(3);
  });

  it("records the chime for the session so its Waiting sound can be skipped", async () => {
    const view = renderHook((p: number) => useApprovalSound(p, "sess-1"), { initialProps: 0 });
    await act(async () => {
      vi.advanceTimersByTime(REPLAY_QUIET_MS);
    });
    expect(approvalChimedRecently("sess-1")).toBe(false);
    view.rerender(1);
    await flushPlayback();
    expect(approvalChimedRecently("sess-1")).toBe(true);
    expect(approvalChimedRecently("other")).toBe(false);
  });

  it("swallows an edge during the replay grace window", async () => {
    const { rerender } = renderHook((p: number) => useApprovalSound(p), { initialProps: 0 });
    rerender(2);
    await flushPlayback();
    expect(playedSounds()).toHaveLength(0);
  });

  it.each([
    ["browser sounds are off", { sound: { enabled: true, browser_enabled: false } }, undefined],
    ["the approval chime is off", { sound: { enabled: true, browser_approval_enabled: false } }, undefined],
    ["sound is disabled", { sound: { enabled: false } }, undefined],
    ["settings have no sound block", {}, undefined],
    ["the sound fetch fails", undefined, () => fetchSound.mockResolvedValue(null)],
  ])("stays silent when %s", async (_label, settings, arrange) => {
    if (settings) fetchSettings.mockResolvedValue(settings);
    arrange?.();
    await walk(1);
    expect(playedSounds()).toHaveLength(0);
  });

  it("stays silent, without recording a chime, while the page has not been interacted with", async () => {
    fakeAudio.state = "suspended";
    fakeAudio.resumable = false;
    const view = renderHook((p: number) => useApprovalSound(p, "sess-blocked"), { initialProps: 0 });
    await act(async () => {
      vi.advanceTimersByTime(REPLAY_QUIET_MS);
    });
    view.rerender(1);
    await flushPlayback();
    await act(async () => {
      vi.advanceTimersByTime(RESUME_WAIT_MS);
    });
    await flushPlayback();
    expect(playedSounds()).toHaveLength(0);
    expect(approvalChimedRecently("sess-blocked")).toBe(false);
  });

  it("caches settings and the decoded sound across plays", async () => {
    await walk(1, 0, 1);
    expect(playedSounds()).toHaveLength(2);
    expect(fetchSettings).toHaveBeenCalledTimes(1);
    expect(fetchSound).toHaveBeenCalledTimes(1);
  });

  it("silences the next edge after a save turns browser sounds off", async () => {
    const { rerender } = await walk(1);
    fetchSettings.mockResolvedValue({ sound: { enabled: true, browser_enabled: false } });
    act(() => notifySettingsChanged());
    for (const count of [0, 1]) {
      rerender(count);
      await flushPlayback();
    }
    expect(fetchSettings).toHaveBeenCalledTimes(2);
    expect(playedSounds()).toHaveLength(1);
  });

  it("does not re-cache a read that started before a settings save", async () => {
    let finishStaleRead: (value: unknown) => void = () => {};
    fetchSettings.mockReturnValueOnce(new Promise((resolve) => (finishStaleRead = resolve)));
    const { rerender } = await walk(1);
    act(() => notifySettingsChanged());
    finishStaleRead({ sound: { enabled: true } });
    await flushPlayback();
    fetchSettings.mockResolvedValue({ sound: { enabled: true, browser_enabled: false } });
    for (const count of [0, 1]) {
      rerender(count);
      await flushPlayback();
    }
    expect(fetchSettings).toHaveBeenCalledTimes(2);
    expect(playedSounds()).toHaveLength(0);
  });
});
