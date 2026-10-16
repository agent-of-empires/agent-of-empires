// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
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

const at = (id: string, status: SessionStatus): SessionResponse => session({ id, status });

async function settle() {
  await act(async () => {
    for (let i = 0; i < 8; i++) await Promise.resolve();
  });
}

/** Mounts the hook on `initial`, then feeds snapshots 3s apart. */
async function walk(initial: SessionResponse[], ...snapshots: SessionResponse[][]) {
  const view = renderHook(({ s }: { s: SessionResponse[] }) => useStatusSounds(s), {
    initialProps: { s: initial },
  });
  for (const s of snapshots) {
    await act(async () => {
      vi.advanceTimersByTime(3000);
    });
    view.rerender({ s });
    await settle();
  }
}

describe("useStatusSounds", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    played = [];
    fetchSettings.mockReset().mockResolvedValue({
      sound: { enabled: true, browser_enabled: true, volume: 1, on_idle: "done", on_waiting: "ask", on_error: "bad" },
    });
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

  it("plays the idle sound once a session settles into Idle", async () => {
    await walk([at("a", "Running")], [at("a", "Idle")], [at("a", "Idle")], [at("a", "Idle")]);
    expect(played).toHaveLength(1);
    expect(fetchSoundBlob).toHaveBeenCalledWith("done");
  });

  it("is silent for sessions already present on load", async () => {
    await walk([at("a", "Idle")], [at("a", "Idle")], [at("a", "Idle")]);
    expect(played).toHaveLength(0);
  });

  it("plays the waiting sound after its longer dwell", async () => {
    await walk([at("a", "Running")], [at("a", "Waiting")], [at("a", "Waiting")]);
    expect(played).toHaveLength(0);
    await walk([at("b", "Running")], [at("b", "Waiting")], [at("b", "Waiting")], [at("b", "Waiting")]);
    expect(fetchSoundBlob).toHaveBeenCalledWith("ask");
  });

  it("plays one sound per event type when several sessions change together", async () => {
    await walk(
      [at("a", "Running"), at("b", "Running"), at("c", "Running")],
      [at("a", "Idle"), at("b", "Idle"), at("c", "Error")],
      [at("a", "Idle"), at("b", "Idle"), at("c", "Error")],
    );
    expect(played).toHaveLength(2);
  });

  it("skips a waiting sound right after the approval chime for that session", async () => {
    noteApprovalChime("a");
    await walk([at("a", "Running")], [at("a", "Waiting")], [at("a", "Waiting")], [at("a", "Waiting")]);
    expect(played).toHaveLength(0);
  });

  it.each([
    ["the master switch is off", { enabled: false, browser_enabled: true }],
    ["browser sounds are off", { enabled: true, browser_enabled: false }],
  ])("is silent when %s", async (_label, sound) => {
    fetchSettings.mockResolvedValue({ sound: { ...sound, on_idle: "done" } });
    await walk([at("a", "Running")], [at("a", "Idle")], [at("a", "Idle")]);
    expect(played).toHaveLength(0);
  });

  it("picks a random sound when none is chosen for the event", async () => {
    fetchSettings.mockResolvedValue({ sound: { enabled: true, browser_enabled: true } });
    await walk([at("a", "Running")], [at("a", "Idle")], [at("a", "Idle")]);
    expect(fetchSoundBlob).toHaveBeenCalledWith("random");
    expect(played).toHaveLength(1);
  });
});
