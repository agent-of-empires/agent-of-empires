// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fakeAudio, installFakeAudioContext, playedSounds, soundBytes } from "./__tests__/fakeAudioContext";

const fetchSettings = vi.fn();
const fetchSound = vi.fn();

vi.mock("./api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchSound: (...args: unknown[]) => fetchSound(...args),
}));

import { clearSoundCache, onSoundBlocked, playSoundFor } from "./soundPlayer";

const ENABLED = { enabled: true, browser_enabled: true, browser_volume: 1 };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

describe("playSoundFor", () => {
  beforeEach(() => {
    installFakeAudioContext();
    fetchSettings.mockReset().mockResolvedValue({ sound: ENABLED });
    fetchSound.mockReset().mockImplementation(async (event: string) => soundBytes(event));
    clearSoundCache();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("plays the built-in sound for the event", async () => {
    expect(await playSoundFor("error")).toBe(true);
    expect(fetchSound).toHaveBeenCalledWith("error");
    expect(playedSounds()).toEqual(["error"]);
  });

  it.each([
    ["the event is switched off", { browser_idle_enabled: false }, {}, false],
    ["another event is switched off", { browser_error_enabled: false }, {}, true],
    [
      "other_sessions and the session is open",
      { browser_scope: "other_sessions" },
      { sessionIds: ["a"], activeSessionId: () => "a" },
      false,
    ],
    [
      "other_sessions and another session is open",
      { browser_scope: "other_sessions" },
      { sessionIds: ["a"], activeSessionId: () => "b" },
      true,
    ],
    [
      "other_sessions and none is open",
      { browser_scope: "other_sessions" },
      { sessionIds: ["a"], activeSessionId: () => null },
      true,
    ],
    ["hidden_tab and the tab is visible", { browser_scope: "hidden_tab" }, {}, false],
  ])("honors the per-event and scope settings when %s", async (_label, extra, context, plays) => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, ...extra } });
    expect(await playSoundFor("idle", context)).toBe(plays);
  });

  it("plays under hidden_tab only while the tab is hidden, and at the browser volume", async () => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_scope: "hidden_tab", browser_volume: 0.4 } });
    const hidden = vi.spyOn(document, "hidden", "get").mockReturnValue(true);
    expect(await playSoundFor("idle")).toBe(true);
    hidden.mockReturnValue(false);
    expect(await playSoundFor("idle")).toBe(false);
    expect(fakeAudio.played).toEqual([{ sound: "idle", gain: 0.4 }]);
  });

  it.each([
    [1.5, 1.5],
    [undefined, 1],
  ])("plays volume %s at gain %s", async (volume, gain) => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_volume: volume } });
    expect(await playSoundFor("idle")).toBe(true);
    expect(fakeAudio.played).toEqual([{ sound: "idle", gain }]);
  });

  it("loads a sound once, and again after a failed load", async () => {
    fetchSound.mockResolvedValueOnce(null);
    expect(await playSoundFor("idle")).toBe(false);
    expect(await playSoundFor("idle")).toBe(true);
    expect(await playSoundFor("idle")).toBe(true);
    expect(fetchSound).toHaveBeenCalledTimes(2);
    expect(playedSounds()).toEqual(["idle", "idle"]);
  });

  it("stays silent until the page is interacted with, and says so once", async () => {
    fakeAudio.state = "suspended";
    fakeAudio.resumable = false;
    const blocked = vi.fn();
    const stop = onSoundBlocked(blocked);
    expect(await playSoundFor("idle")).toBe(false);
    expect(await playSoundFor("waiting")).toBe(false);
    expect(blocked).toHaveBeenCalledTimes(1);
    stop();

    // A gesture resumes the context, so the next sound plays.
    fakeAudio.resumable = true;
    window.dispatchEvent(new Event("pointerdown"));
    await vi.waitFor(() => expect(fakeAudio.state).toBe("running"));
    expect(await playSoundFor("idle")).toBe(true);
    expect(playedSounds()).toEqual(["idle"]);
  });

  it("plays nothing when settings are saved while they are loading", async () => {
    const settings = deferred<unknown>();
    fetchSettings.mockReturnValueOnce(settings.promise);
    const first = playSoundFor("idle");
    await vi.waitFor(() => expect(fetchSettings).toHaveBeenCalledTimes(1));

    clearSoundCache();
    settings.resolve({ sound: ENABLED });

    expect(await first).toBe(false);
    expect(fetchSound).not.toHaveBeenCalled();
    expect(playedSounds()).toEqual([]);
  });

  it("plays nothing when settings are saved while the sound is loading", async () => {
    const bytes = deferred<ArrayBuffer>();
    fetchSound.mockReturnValueOnce(bytes.promise);
    const first = playSoundFor("idle");
    await vi.waitFor(() => expect(fetchSound).toHaveBeenCalledTimes(1));

    clearSoundCache();
    bytes.resolve(soundBytes("idle"));

    expect(await first).toBe(false);
    expect(playedSounds()).toEqual([]);
  });
});
