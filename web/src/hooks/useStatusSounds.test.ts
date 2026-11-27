// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, type RenderHookResult } from "@testing-library/react";
import type { SessionResponse, SessionStatus } from "../lib/types";
import { session } from "./__tests__/fixtures";
import { fakeAudio, installFakeAudioContext, playedSounds, soundBytes } from "../lib/__tests__/fakeAudioContext";

const fetchSettings = vi.fn();
const fetchSound = vi.fn();

vi.mock("../lib/api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchSound: (...args: unknown[]) => fetchSound(...args),
}));

import { useStatusSounds } from "./useStatusSounds";
import { clearSoundCache, noteApprovalChime } from "../lib/soundPlayer";
import { notifySettingsChanged } from "../lib/settingsEvents";

const ENABLED = { enabled: true, browser_enabled: true, browser_volume: 1 };

let view: RenderHookResult<ReturnType<typeof useStatusSounds>, { s: SessionResponse[] }>;
let activeId: string | null = null;

const at = (id: string, status: SessionStatus, over: Partial<SessionResponse> = {}): SessionResponse =>
  session({ id, status, ...over });

/** Mounts the hook on `initial`; `step` then feeds one session list refresh, 3s later. */
function mount(initial: SessionResponse[]) {
  view = renderHook(({ s }: { s: SessionResponse[] }) => useStatusSounds(s, activeId), {
    initialProps: { s: initial },
  });
}

async function step(sessions: SessionResponse[]) {
  await act(async () => {
    vi.advanceTimersByTime(3000);
  });
  view.rerender({ s: [...sessions] }); // each real refresh is a new array
}

/**
 * Proves the sound pipeline had time to run: enables sounds, walks a fresh session into Idle and waits for its sound.
 * Any sound the case under test wrongly played started earlier and would have played first, so after `prior` the
 * probe's must be the only one.
 */
async function expectOnlyProbeSounds(base: SessionResponse[], prior: string[] = []) {
  fetchSettings.mockResolvedValue({ sound: ENABLED });
  act(() => notifySettingsChanged());
  await step([...base, at("probe", "Running")]);
  await step([...base, at("probe", "Idle")]);
  await step([...base, at("probe", "Idle")]);
  await vi.waitFor(() => expect(playedSounds()).toHaveLength(prior.length + 1));
  expect(playedSounds()).toEqual([...prior, "idle"]);
}

describe("useStatusSounds", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    installFakeAudioContext();
    activeId = null;
    fetchSettings.mockReset().mockResolvedValue({ sound: ENABLED });
    fetchSound.mockReset().mockImplementation(async (event: string) => soundBytes(event));
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
    act(() => view.result.current.onSoundDue("a", "waiting"));
    await vi.waitFor(() => expect(playedSounds()).toEqual(["waiting"]));
    await step([at("a", "Waiting")]);
    await step([at("a", "Waiting")]);
    await expectOnlyProbeSounds([at("a", "Waiting")], ["waiting"]);
  });

  it("replays a cue that arrived before the refreshed list, without waiting for another poll", async () => {
    mount([at("a", "Running")]);
    // The reconnect snapshot is still in flight, so the list still shows the stale status.
    act(() => view.result.current.onSoundDue("a", "idle"));
    await step([at("a", "Idle")]);
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
    await step([at("a", "Idle")]);
    await step([at("a", "Idle")]);
    await expectOnlyProbeSounds([at("a", "Idle")], ["idle"]);
  });

  it("keeps a cue across a stale snapshot, and drops it once the session moves to another status", async () => {
    mount([at("a", "Running"), at("b", "Running")]);
    act(() => {
      view.result.current.onSoundDue("a", "idle");
      view.result.current.onSoundDue("b", "idle");
    });
    await step([at("a", "Running"), at("b", "Running")]);
    await step([at("a", "Idle"), at("b", "Error")]);
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
    expect(playedSounds()).toEqual(["idle"]);
    // b's cue is gone: going Idle later waits for its own dwell instead of sounding at once.
    await step([at("a", "Idle"), at("b", "Idle")]);
    expect(playedSounds()).toHaveLength(1);
  });

  it("drops a held cue when a status push starts a new interval", async () => {
    mount([at("a", "Running")]);
    act(() => view.result.current.onSoundDue("a", "idle")); // held against the stale Running row
    act(() => view.result.current.cancelCue("a")); // a Running push leaves the row unchanged
    await step([at("a", "Running")]);
    await step([at("a", "Idle")]); // the new Idle interval has not dwelt yet
    await expectOnlyProbeSounds([at("a", "Idle")]);
  });

  it("stays silent for a late cue about a session first seen by the refreshed list", async () => {
    mount([at("a", "Running")]);
    act(() => view.result.current.onSoundDue("new", "waiting"));
    await step([at("a", "Running"), at("new", "Waiting")]);
    await expectOnlyProbeSounds([at("a", "Running"), at("new", "Waiting")]);
  });

  it("plays one sound when several sessions cue the same event together, and a later cue still sounds", async () => {
    mount([at("a", "Running"), at("b", "Running"), at("c", "Running")]);
    await step([at("a", "Idle"), at("b", "Idle"), at("c", "Idle")]);
    act(() => {
      view.result.current.onSoundDue("a", "idle");
      view.result.current.onSoundDue("b", "idle");
    });
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
    await act(async () => {
      vi.advanceTimersByTime(1000);
    });
    act(() => view.result.current.onSoundDue("c", "idle"));
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(2));
  });

  it("cues a reconnect mid-dwell once, and stays silent for a status that was there at load", async () => {
    mount([at("old", "Waiting"), at("a", "Running")]);
    await step([at("old", "Waiting"), at("a", "Waiting")]);
    // A new socket's seeded cues: one for the observed transition, one for the baseline status.
    act(() => {
      view.result.current.onSoundDue("a", "waiting");
      view.result.current.onSoundDue("old", "waiting");
      view.result.current.onSoundDue("a", "waiting");
    });
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
    await step([at("old", "Waiting"), at("a", "Waiting")]);
    expect(playedSounds()).toHaveLength(1);
  });

  it("plays the idle sound once a session settles into Idle", async () => {
    mount([at("a", "Running")]);
    await step([at("a", "Idle")]);
    await step([at("a", "Idle")]);
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
    expect(playedSounds()).toEqual(["idle"]);
  });

  it("plays the waiting sound after its longer dwell", async () => {
    mount([at("a", "Running")]);
    await step([at("a", "Waiting")]);
    await step([at("a", "Waiting")]);
    await step([at("a", "Waiting")]);
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
    expect(playedSounds()).toEqual(["waiting"]);
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
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(2));
    expect(playedSounds().sort()).toEqual(["error", "idle"]);
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
    fetchSettings.mockResolvedValue({ sound });
    const idle = [at("a", "Idle")];
    mount([at("a", "Running")]);
    await step(idle);
    await step(idle);
    await expectOnlyProbeSounds(idle);
  });

  it("skips the open session under other_sessions and coalesces what is left", async () => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_scope: "other_sessions" } });
    activeId = "a";
    const changed = [at("a", "Idle"), at("b", "Idle"), at("c", "Idle"), at("d", "Error")];
    mount(changed.map((s) => at(s.id, "Running")));
    await step(changed);
    await step(changed);
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(2));
    expect(playedSounds().sort()).toEqual(["error", "idle"]);
  });

  it("stays silent under other_sessions when the only change is the open session", async () => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_scope: "other_sessions" } });
    activeId = "a";
    mount([at("a", "Running")]);
    await step([at("a", "Idle")]);
    await step([at("a", "Idle")]);
    await expectOnlyProbeSounds([at("a", "Idle")]);
  });

  it("stays silent under other_sessions when the session is opened while its sound loads", async () => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_scope: "other_sessions" } });
    let resolveBytes!: (bytes: ArrayBuffer) => void;
    fetchSound.mockReturnValueOnce(new Promise<ArrayBuffer>((r) => (resolveBytes = r)));
    activeId = "a";
    mount([at("a", "Running"), at("b", "Running")]);
    const changed = [at("a", "Running"), at("b", "Idle")];
    await step(changed);
    await step(changed);
    await vi.waitFor(() => expect(fetchSound).toHaveBeenCalledWith("idle"));

    activeId = "b";
    view.rerender({ s: [...changed] });
    await act(async () => {
      resolveBytes(soundBytes("idle"));
    });
    await vi.waitFor(() => expect(fakeAudio.decoded).toBe(1));
    await act(async () => {
      for (let i = 0; i < 8; i++) await Promise.resolve();
    });
    expect(playedSounds()).toHaveLength(0);
  });

  it("still plays when the first of several due sessions is opened while the sound loads", async () => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_scope: "other_sessions" } });
    let resolveBytes!: (bytes: ArrayBuffer) => void;
    fetchSound.mockReturnValueOnce(new Promise<ArrayBuffer>((r) => (resolveBytes = r)));
    activeId = "a";
    mount([at("a", "Running"), at("b", "Running"), at("c", "Running")]);
    const changed = [at("a", "Running"), at("b", "Idle"), at("c", "Idle")];
    await step(changed);
    await step(changed);
    await vi.waitFor(() => expect(fetchSound).toHaveBeenCalledWith("idle"));

    activeId = "b";
    view.rerender({ s: [...changed] });
    await act(async () => {
      resolveBytes(soundBytes("idle"));
    });
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
  });

  it("lets a cue sound right after a scope-dropped one for the same event", async () => {
    fetchSettings.mockResolvedValue({ sound: { ...ENABLED, browser_scope: "other_sessions" } });
    activeId = "a";
    mount([at("a", "Running"), at("b", "Running")]);
    await step([at("a", "Idle"), at("b", "Idle")]);
    act(() => view.result.current.onSoundDue("a", "idle"));
    await act(async () => {
      for (let i = 0; i < 8; i++) await Promise.resolve();
    });
    expect(playedSounds()).toHaveLength(0);
    act(() => view.result.current.onSoundDue("b", "idle"));
    await vi.waitFor(() => expect(playedSounds()).toHaveLength(1));
  });

  it("is silent for a session muted for this event", async () => {
    const muted = { notify_on_idle: false };
    mount([at("a", "Running", muted)]);
    await step([at("a", "Idle", muted)]);
    await step([at("a", "Idle", muted)]);
    await expectOnlyProbeSounds([at("a", "Idle", muted)]);
  });
});
