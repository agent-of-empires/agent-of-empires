// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, type RenderHookResult } from "@testing-library/react";
import type { SessionResponse, SessionStatus } from "../lib/types";
import { session } from "./__tests__/fixtures";

const fetchSettings = vi.fn();
const fetchSounds = vi.fn();
const fetchSoundBlob = vi.fn();

vi.mock("../lib/api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchSounds: (...args: unknown[]) => fetchSounds(...args),
  fetchSoundBlob: (...args: unknown[]) => fetchSoundBlob(...args),
}));

import { useStatusSounds } from "./useStatusSounds";
import { clearSoundCache, noteApprovalChime } from "../lib/soundPlayer";
import { notifySettingsChanged } from "../lib/settingsEvents";

const ENABLED = {
  enabled: true,
  browser_enabled: true,
  volume: 1,
  on_idle: "done",
  on_waiting: "ask",
  on_error: "bad",
};

let played: string[] = [];
let view: RenderHookResult<ReturnType<typeof useStatusSounds>, { s: SessionResponse[] }>;

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

const at = (id: string, status: SessionStatus, over: Partial<SessionResponse> = {}): SessionResponse =>
  session({ id, status, ...over });

/** Mounts the hook on `initial`; `step` then feeds one session list refresh, 3s later. */
function mount(initial: SessionResponse[]) {
  view = renderHook(({ s }: { s: SessionResponse[] }) => useStatusSounds(s), { initialProps: { s: initial } });
}

async function step(sessions: SessionResponse[]) {
  await act(async () => {
    vi.advanceTimersByTime(3000);
  });
  view.rerender({ s: [...sessions] }); // each real refresh is a new array
}

/**
 * Proves the sound pipeline had time to run: enables sounds with a distinct probe sound, walks a fresh session into
 * Idle and waits for the probe to load. Any sound the case under test wrongly played would have fetched its own file
 * first, so the fetched names must be exactly the probe.
 */
async function expectOnlyProbeSounds(base: SessionResponse[]) {
  fetchSettings.mockResolvedValue({ sound: { ...ENABLED, on_idle: "probe" } });
  act(() => notifySettingsChanged());
  await step([...base, at("probe", "Running")]);
  await step([...base, at("probe", "Idle")]);
  await step([...base, at("probe", "Idle")]);
  await vi.waitFor(() => expect(fetchSoundBlob).toHaveBeenCalledWith("probe"));
  await vi.waitFor(() => expect(played).toHaveLength(1));
  expect(fetchSoundBlob.mock.calls.map(([name]) => name)).toEqual(["probe"]);
}

describe("useStatusSounds", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    played = [];
    fetchSettings.mockReset().mockResolvedValue({ sound: ENABLED });
    fetchSounds.mockReset().mockResolvedValue(["random"]);
    fetchSoundBlob.mockReset().mockImplementation(async (name: string) => new Blob([name]));
    vi.stubGlobal("Audio", FakeAudio);
    let n = 0;
    vi.spyOn(URL, "createObjectURL").mockImplementation(() => `blob:${n++}`);
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    clearSoundCache();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("plays a server cue without waiting for a refresh, and the next refresh does not repeat it", async () => {
    mount([at("a", "Running")]);
    await step([at("a", "Waiting")]);
    act(() => view.result.current("a", "waiting"));
    await vi.waitFor(() => expect(played).toEqual(["blob:0"]));
    await step([at("a", "Waiting")]);
    await step([at("a", "Waiting")]);
    await vi.waitFor(() => expect(fetchSoundBlob).toHaveBeenCalledTimes(1));
    expect(played).toHaveLength(1);
  });

  it("plays the idle sound once a session settles into Idle", async () => {
    mount([at("a", "Running")]);
    await step([at("a", "Idle")]);
    await step([at("a", "Idle")]);
    await vi.waitFor(() => expect(played).toHaveLength(1));
    expect(fetchSoundBlob).toHaveBeenCalledWith("done");
  });

  it("plays the waiting sound after its longer dwell", async () => {
    mount([at("a", "Running")]);
    await step([at("a", "Waiting")]);
    await step([at("a", "Waiting")]);
    await step([at("a", "Waiting")]);
    await vi.waitFor(() => expect(played).toHaveLength(1));
    expect(fetchSoundBlob).toHaveBeenCalledWith("ask");
  });

  it("is silent for sessions already present on load", async () => {
    mount([at("a", "Idle")]);
    await step([at("a", "Idle")]);
    await step([at("a", "Idle")]);
    await expectOnlyProbeSounds([at("a", "Idle")]);
  });

  it("plays one sound per event type when several sessions change together", async () => {
    const changed = [at("a", "Idle"), at("b", "Idle"), at("c", "Error")];
    mount([at("a", "Running"), at("b", "Running"), at("c", "Running")]);
    await step(changed);
    await step(changed);
    await vi.waitFor(() => expect(played).toHaveLength(2));
    expect(fetchSoundBlob.mock.calls.map(([name]) => name).sort()).toEqual(["bad", "done"]);
  });

  it("skips a waiting sound right after the approval chime for that session", async () => {
    noteApprovalChime("a");
    const waiting = [at("a", "Waiting")];
    mount([at("a", "Running")]);
    await step(waiting);
    await step(waiting);
    await step(waiting);
    await expectOnlyProbeSounds(waiting);
  });

  it.each([
    ["the master switch is off", { enabled: false, browser_enabled: true }],
    ["browser sounds are off", { enabled: true, browser_enabled: false }],
  ])("is silent when %s", async (_label, sound) => {
    fetchSettings.mockResolvedValue({ sound: { ...sound, on_idle: "done" } });
    const idle = [at("a", "Idle")];
    mount([at("a", "Running")]);
    await step(idle);
    await step(idle);
    await expectOnlyProbeSounds(idle);
  });

  it("is silent for a session muted for this event", async () => {
    const muted = { notify_on_idle: false };
    mount([at("a", "Running", muted)]);
    await step([at("a", "Idle", muted)]);
    await step([at("a", "Idle", muted)]);
    await expectOnlyProbeSounds([at("a", "Idle", muted)]);
  });

  it("loads a shared sound file once when several events use it", async () => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, on_idle: "same", on_error: "same" } });
    const changed = [at("a", "Idle"), at("b", "Error")];
    mount([at("a", "Running"), at("b", "Running")]);
    await step(changed);
    await step(changed);
    await vi.waitFor(() => expect(played).toHaveLength(2));
    expect(fetchSoundBlob).toHaveBeenCalledTimes(1);
    expect(URL.createObjectURL).toHaveBeenCalledTimes(1);
  });

  it("picks a random sound when none is chosen for the event", async () => {
    fetchSettings.mockResolvedValue({ sound: { enabled: true, browser_enabled: true } });
    mount([at("a", "Running")]);
    await step([at("a", "Idle")]);
    await step([at("a", "Idle")]);
    await vi.waitFor(() => expect(played).toHaveLength(1));
    expect(fetchSoundBlob).toHaveBeenCalledWith("random");
  });
});
