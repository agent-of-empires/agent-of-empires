// @vitest-environment jsdom
//
// Unit tests for useApprovalSound. Covers the 0 -> >=1 pending-approval
// edge that plays the configured chime, the replay-quiet grace window
// that swallows the initial-load case, settings caching/TTL, sound-name
// resolution (explicit override / specific / random), the disabled and
// missing-blob short-circuits, the autoplay-rejection swallow, and the
// cache-clear path used by logout.
//
// The api layer (fetchSettings / fetchSounds / fetchSoundBlob) is mocked,
// and window.Audio is replaced with a recording stub so playback is
// observable without real audio.

import { describe, expect, it, beforeEach, afterEach, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

const fetchSettings = vi.fn();
const fetchSounds = vi.fn();
const fetchSoundBlob = vi.fn();

vi.mock("../lib/api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchSounds: (...args: unknown[]) => fetchSounds(...args),
  fetchSoundBlob: (...args: unknown[]) => fetchSoundBlob(...args),
}));

import { useApprovalSound, clearApprovalSoundCache } from "./useApprovalSound";

const REPLAY_QUIET_MS = 1500;

interface AudioInstance {
  src: string;
  volume: number;
  play: ReturnType<typeof vi.fn>;
}

let audioInstances: AudioInstance[] = [];
let playImpl: () => Promise<void>;

class FakeAudio {
  src: string;
  volume = 1;
  play: ReturnType<typeof vi.fn>;
  constructor(src: string) {
    this.src = src;
    this.play = vi.fn(() => playImpl());
    audioInstances.push(this as unknown as AudioInstance);
  }
}

// Drain the two layered async hops: the setTimeout(0) playback timer and
// the awaited promise chain inside playApprovalSound.
async function flushPlayback(): Promise<void> {
  await act(async () => {
    vi.advanceTimersByTime(0);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("useApprovalSound", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    audioInstances = [];
    playImpl = () => Promise.resolve();
    fetchSettings.mockReset();
    fetchSounds.mockReset();
    fetchSoundBlob.mockReset();
    fetchSettings.mockResolvedValue({
      sound: { enabled: true, volume: 1.0, on_approval: "ding" },
    });
    fetchSounds.mockResolvedValue(["ding", "chime"]);
    fetchSoundBlob.mockResolvedValue(new Blob(["audio"], { type: "audio/wav" }));
    vi.stubGlobal("Audio", FakeAudio as unknown as typeof Audio);
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:fake-url");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    clearApprovalSoundCache();
    // clearApprovalSoundCache may have triggered a revoke spy call; reset.
    (URL.revokeObjectURL as ReturnType<typeof vi.fn>).mockClear();
  });

  afterEach(() => {
    vi.runOnlyPendingTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  // Crosses the replay-quiet grace window, then drives a 0 -> 1 edge.
  async function mountPastQuietPeriod(initial = 0) {
    const view = renderHook((p: number) => useApprovalSound(p), {
      initialProps: initial,
    });
    await act(async () => {
      vi.advanceTimersByTime(REPLAY_QUIET_MS);
    });
    return view;
  }

  it("plays the configured chime on a 0 -> >=1 edge after the grace window", async () => {
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();

    expect(audioInstances).toHaveLength(1);
    expect(audioInstances[0].src).toBe("blob:fake-url");
    expect(audioInstances[0].play).toHaveBeenCalledTimes(1);
    expect(fetchSoundBlob).toHaveBeenCalledWith("ding");
  });

  it("swallows a 0 -> >=1 edge during the replay-quiet grace window", async () => {
    const { rerender } = renderHook((p: number) => useApprovalSound(p), {
      initialProps: 0,
    });
    // Edge arrives before the quiet timer fires -> no chime.
    rerender(2);
    await flushPlayback();
    expect(audioInstances).toHaveLength(0);
  });

  it("does not replay on >=1 -> higher transitions (only the 0 edge fires)", async () => {
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances).toHaveLength(1);

    // 1 -> 3 is not a 0-edge; no new chime.
    rerender(3);
    await flushPlayback();
    expect(audioInstances).toHaveLength(1);
  });

  it("plays again on a fresh 0 -> >=1 edge after dropping back to 0", async () => {
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    rerender(0);
    await flushPlayback();
    rerender(1);
    await flushPlayback();
    expect(audioInstances).toHaveLength(2);
  });

  it("does nothing when sound is disabled", async () => {
    fetchSettings.mockResolvedValue({ sound: { enabled: false } });
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances).toHaveLength(0);
  });

  it("does nothing when settings have no sound block", async () => {
    fetchSettings.mockResolvedValue({});
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances).toHaveLength(0);
  });

  it("resolves a specific sound from mode.specific when no override", async () => {
    fetchSettings.mockResolvedValue({
      sound: { enabled: true, mode: { specific: "chime" } },
    });
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(fetchSoundBlob).toHaveBeenCalledWith("chime");
  });

  it("picks a random sound from the list in random mode", async () => {
    fetchSettings.mockResolvedValue({
      sound: { enabled: true, mode: "random" },
    });
    vi.spyOn(Math, "random").mockReturnValue(0.99);
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(fetchSounds).toHaveBeenCalled();
    // 0.99 * 2 -> index 1 -> "chime".
    expect(fetchSoundBlob).toHaveBeenCalledWith("chime");
  });

  it("does nothing in random mode when the sound list is empty", async () => {
    fetchSettings.mockResolvedValue({
      sound: { enabled: true, mode: "random" },
    });
    fetchSounds.mockResolvedValue([]);
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances).toHaveLength(0);
  });

  it("does nothing when no sound name resolves", async () => {
    fetchSettings.mockResolvedValue({
      sound: { enabled: true, on_approval: "   " },
    });
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances).toHaveLength(0);
  });

  it("does nothing when the blob fetch returns null", async () => {
    fetchSoundBlob.mockResolvedValue(null);
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances).toHaveLength(0);
  });

  it("clamps volume into the 0..1 HTMLAudioElement range", async () => {
    fetchSettings.mockResolvedValue({
      sound: { enabled: true, on_approval: "ding", volume: 1.5 },
    });
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances[0].volume).toBe(1);
  });

  it("defaults volume to 1.0 when unset", async () => {
    fetchSettings.mockResolvedValue({
      sound: { enabled: true, on_approval: "ding" },
    });
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(audioInstances[0].volume).toBe(1);
  });

  it("swallows an autoplay-policy rejection from play()", async () => {
    playImpl = () => Promise.reject(new Error("autoplay blocked"));
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    // Must not surface an unhandled rejection.
    await flushPlayback();
    expect(audioInstances[0].play).toHaveBeenCalledTimes(1);
  });

  it("caches settings within the TTL (only one fetchSettings per window)", async () => {
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    rerender(0);
    await flushPlayback();
    rerender(1);
    await flushPlayback();
    // Two plays, but settings fetched once thanks to the 30s TTL cache.
    expect(audioInstances).toHaveLength(2);
    expect(fetchSettings).toHaveBeenCalledTimes(1);
  });

  it("reuses the cached blob URL for the same sound name", async () => {
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    rerender(0);
    await flushPlayback();
    rerender(1);
    await flushPlayback();
    // Same name "ding" both times -> blob fetched once, URL created once.
    expect(fetchSoundBlob).toHaveBeenCalledTimes(1);
    expect(URL.createObjectURL).toHaveBeenCalledTimes(1);
  });

  it("clearApprovalSoundCache revokes the blob URL and forces a refetch", async () => {
    const { rerender } = await mountPastQuietPeriod(0);
    rerender(1);
    await flushPlayback();
    expect(fetchSettings).toHaveBeenCalledTimes(1);

    act(() => {
      clearApprovalSoundCache();
    });
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:fake-url");

    rerender(0);
    await flushPlayback();
    rerender(1);
    await flushPlayback();
    // Cache dropped -> settings and blob fetched again.
    expect(fetchSettings).toHaveBeenCalledTimes(2);
    expect(fetchSoundBlob).toHaveBeenCalledTimes(2);
  });

  it("clearApprovalSoundCache is a no-op when no sound is cached", () => {
    expect(() => clearApprovalSoundCache()).not.toThrow();
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();
  });
});
