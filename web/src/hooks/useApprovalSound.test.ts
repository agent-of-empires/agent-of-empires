// @vitest-environment jsdom

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

let audioInstances: FakeAudio[] = [];
let playImpl: () => Promise<void>;

class FakeAudio {
  volume = 1;
  play = vi.fn(() => playImpl());
  src: string;
  constructor(src: string) {
    this.src = src;
    audioInstances.push(this);
  }
}

async function flushPlayback(): Promise<void> {
  await act(async () => {
    vi.advanceTimersByTime(0);
    for (let i = 0; i < 4; i++) await Promise.resolve();
  });
}

describe("useApprovalSound", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    audioInstances = [];
    playImpl = () => Promise.resolve();
    fetchSettings.mockReset().mockResolvedValue({ sound: { enabled: true, volume: 1.0, on_approval: "ding" } });
    fetchSounds.mockReset().mockResolvedValue(["ding", "chime"]);
    fetchSoundBlob.mockReset().mockResolvedValue(new Blob(["audio"], { type: "audio/wav" }));
    vi.stubGlobal("Audio", FakeAudio);
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:fake-url");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    clearApprovalSoundCache();
    vi.mocked(URL.revokeObjectURL).mockClear();
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

  it("plays the configured sound once per 0 -> >=1 edge after the grace window", async () => {
    await walk(1, 3);
    expect(audioInstances).toHaveLength(1);
    expect(audioInstances[0]!.src).toBe("blob:fake-url");
    expect(audioInstances[0]!.play).toHaveBeenCalledTimes(1);
    expect(fetchSoundBlob).toHaveBeenCalledWith("ding");

    await walk(1, 0, 1);
    expect(audioInstances).toHaveLength(3);
  });

  it("swallows an edge during the replay grace window", async () => {
    const { rerender } = renderHook((p: number) => useApprovalSound(p), { initialProps: 0 });
    rerender(2);
    await flushPlayback();
    expect(audioInstances).toHaveLength(0);
  });

  it.each([
    ["sound is disabled", { sound: { enabled: false } }, undefined],
    ["settings have no sound block", {}, undefined],
    ["no sound name resolves", { sound: { enabled: true, on_approval: "   " } }, undefined],
    [
      "random mode has no sounds",
      { sound: { enabled: true, mode: "random" } },
      () => fetchSounds.mockResolvedValue([]),
    ],
    ["the blob fetch returns null", undefined, () => fetchSoundBlob.mockResolvedValue(null)],
  ])("stays silent when %s", async (_label, settings, arrange) => {
    if (settings) fetchSettings.mockResolvedValue(settings);
    arrange?.();
    await walk(1);
    expect(audioInstances).toHaveLength(0);
  });

  it.each([
    ["mode.specific", { mode: { specific: "chime" } }],
    ["a random pick", { mode: "random" }],
  ])("resolves the sound from %s", async (_label, sound) => {
    fetchSettings.mockResolvedValue({ sound: { enabled: true, ...sound } });
    vi.spyOn(Math, "random").mockReturnValue(0.99);
    await walk(1);
    expect(fetchSoundBlob).toHaveBeenCalledWith("chime");
  });

  it.each([1.5, undefined])("clamps volume %s into the 0..1 audio range", async (volume) => {
    fetchSettings.mockResolvedValue({ sound: { enabled: true, on_approval: "ding", volume } });
    await walk(1);
    expect(audioInstances[0]!.volume).toBe(1);
  });

  it("swallows an autoplay-policy rejection", async () => {
    playImpl = () => Promise.reject(new Error("autoplay blocked"));
    await walk(1);
    expect(audioInstances[0]!.play).toHaveBeenCalledTimes(1);
  });

  it("caches settings and the blob URL across plays", async () => {
    await walk(1, 0, 1);
    expect(audioInstances).toHaveLength(2);
    expect(fetchSettings).toHaveBeenCalledTimes(1);
    expect(fetchSoundBlob).toHaveBeenCalledTimes(1);
    expect(URL.createObjectURL).toHaveBeenCalledTimes(1);
  });

  it("clearApprovalSoundCache revokes the blob URL and forces a refetch", async () => {
    clearApprovalSoundCache();
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();

    const { rerender } = await walk(1);
    act(() => clearApprovalSoundCache());
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:fake-url");
    for (const count of [0, 1]) {
      rerender(count);
      await flushPlayback();
    }
    expect(fetchSettings).toHaveBeenCalledTimes(2);
    expect(fetchSoundBlob).toHaveBeenCalledTimes(2);
  });
});
