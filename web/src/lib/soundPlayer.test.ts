// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const fetchSettings = vi.fn();
const fetchSounds = vi.fn();
const fetchSoundBlob = vi.fn();

vi.mock("./api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchSounds: (...args: unknown[]) => fetchSounds(...args),
  fetchSoundBlob: (...args: unknown[]) => fetchSoundBlob(...args),
}));

import { clearSoundCache, playSoundFor } from "./soundPlayer";

const ENABLED = { enabled: true, browser_enabled: true, volume: 1 };

let played: string[] = [];

class FakeAudio {
  volume = 1;
  src: string;
  constructor(src: string) {
    this.src = src;
  }
  play() {
    played.push(this.src);
    return Promise.resolve();
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

describe("playSoundFor", () => {
  beforeEach(() => {
    played = [];
    fetchSettings.mockReset().mockResolvedValue({ sound: ENABLED });
    fetchSounds.mockReset().mockResolvedValue(["x.wav"]);
    fetchSoundBlob.mockReset().mockImplementation(async (name: string) => new Blob([name]));
    vi.stubGlobal("Audio", FakeAudio);
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:fake");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    clearSoundCache();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("retries discovery after an empty or failed sound list", async () => {
    fetchSounds.mockResolvedValueOnce([]);
    expect(await playSoundFor("idle")).toBe(false);
    expect(await playSoundFor("idle")).toBe(true);
    expect(fetchSounds).toHaveBeenCalledTimes(2);
    expect(await playSoundFor("idle")).toBe(true);
    expect(fetchSounds).toHaveBeenCalledTimes(2);
    expect(played).toHaveLength(2);
  });

  it("plays nothing when settings are saved while discovery is in flight", async () => {
    const discovery = deferred<string[]>();
    fetchSounds.mockReturnValueOnce(discovery.promise);
    const first = playSoundFor("idle");
    await vi.waitFor(() => expect(fetchSounds).toHaveBeenCalledTimes(1));

    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_enabled: false } });
    clearSoundCache();
    discovery.resolve(["x.wav"]);

    expect(await first).toBe(false);
    expect(fetchSoundBlob).not.toHaveBeenCalled();
    expect(played).toHaveLength(0);

    // The stale list was not published, so re-enabling asks again.
    fetchSettings.mockResolvedValue({ sound: ENABLED });
    clearSoundCache();
    expect(await playSoundFor("idle")).toBe(true);
    expect(fetchSounds).toHaveBeenCalledTimes(2);
  });

  it("plays nothing when settings are saved while they are loading", async () => {
    const settings = deferred<unknown>();
    fetchSettings.mockReturnValueOnce(settings.promise);
    const first = playSoundFor("idle");
    await vi.waitFor(() => expect(fetchSettings).toHaveBeenCalledTimes(1));

    clearSoundCache();
    settings.resolve({ sound: ENABLED });

    expect(await first).toBe(false);
    expect(fetchSoundBlob).not.toHaveBeenCalled();
    expect(played).toHaveLength(0);
  });

  it("plays nothing when settings are saved while the sound file is loading", async () => {
    const blob = deferred<Blob>();
    fetchSoundBlob.mockReturnValueOnce(blob.promise);
    const first = playSoundFor("idle");
    await vi.waitFor(() => expect(fetchSoundBlob).toHaveBeenCalledTimes(1));

    clearSoundCache();
    blob.resolve(new Blob(["x"]));

    expect(await first).toBe(false);
    expect(URL.createObjectURL).not.toHaveBeenCalled();
    expect(played).toHaveLength(0);
  });
});
