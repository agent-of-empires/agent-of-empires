// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";

vi.mock("./api", () => ({ fetchSettings: vi.fn() }));

import { fetchSettings, type SaveOutcome } from "./api";
import { loadSettings, resetSettingsStore, saveField, useProfileSettings, type SettingsView } from "./settingsStore";

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

const plugins = (list: string[]) => ({ web: { list } });
const listOf = (hook: { result: { current: SettingsView } }) =>
  (hook.result.current.settings?.web as { list: string[] } | undefined)?.list;

beforeEach(() => {
  resetSettingsStore();
  vi.mocked(fetchSettings).mockReset();
});

async function seed(profile: string, list: string[]) {
  vi.mocked(fetchSettings).mockResolvedValueOnce(plugins(list));
  loadSettings(profile);
  const hook = renderHook(() => useProfileSettings(profile));
  await waitFor(() => expect(hook.result.current.settings).not.toBeNull());
  return hook;
}

describe("settings store", () => {
  it("composes two removals across a remount and sends them in order", async () => {
    const first = await seed("main", ["a", "b", "c"]);
    const a = deferred<SaveOutcome>();
    const sendA = vi.fn(() => a.promise);
    const sendB = vi.fn(async (): Promise<SaveOutcome> => "committed");

    void saveField("main", "web", "list", ["b", "c"], sendA);
    first.unmount();

    // A remounted widget reads the edit still waiting on its PATCH, so removing `b` yields `[c]`.
    const second = renderHook(() => useProfileSettings("main"));
    expect(listOf(second)).toEqual(["b", "c"]);
    const next = (listOf(second) ?? []).filter((x) => x !== "b");
    void saveField("main", "web", "list", next, sendB);
    expect(sendB).not.toHaveBeenCalled();

    await act(async () => a.resolve("committed"));
    await waitFor(() => expect(sendB).toHaveBeenCalledTimes(1));
    expect(listOf(second)).toEqual(["c"]);
  });

  it("keeps an edit the server persisted but reported as uncertain, then reconciles", async () => {
    const hook = await seed("main", ["a", "b", "c"]);
    const reconcile = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings).mockReturnValueOnce(reconcile.promise);

    await act(async () => {
      await saveField("main", "web", "list", ["b", "c"], async () => "uncertain");
    });
    expect(listOf(hook)).toEqual(["b", "c"]);
    expect(hook.result.current.error).toMatch(/Could not confirm/);
    expect(fetchSettings).toHaveBeenCalledTimes(2);

    await act(async () => reconcile.resolve(plugins(["b", "c"])));
    expect(listOf(hook)).toEqual(["b", "c"]);
  });

  it("reports a rejected save and restores the server value", async () => {
    const hook = await seed("main", ["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValueOnce(plugins(["a", "b"]));
    let ok = true;
    await act(async () => {
      ok = await saveField("main", "web", "list", ["b"], async () => "rejected");
    });
    expect(ok).toBe(false);
    expect(hook.result.current.error).toBe("Failed to save, please try again");
    await waitFor(() => expect(listOf(hook)).toEqual(["a", "b"]));
  });

  it("keeps the loaded settings when a reconcile GET rejects", async () => {
    const hook = await seed("main", ["a", "b"]);
    vi.mocked(fetchSettings).mockRejectedValueOnce(new Error("offline"));
    await act(async () => {
      await saveField("main", "web", "list", ["b"], async () => "uncertain");
    });
    await waitFor(() => expect(hook.result.current.error).toBe("Could not refresh settings, please reload"));
    expect(listOf(hook)).toEqual(["b"]);
  });

  it("does not refetch after a committed save", async () => {
    await seed("main", ["a", "b"]);
    await act(async () => {
      await saveField("main", "web", "list", ["b"], async () => "committed");
    });
    expect(fetchSettings).toHaveBeenCalledTimes(1);
  });

  it("drops a GET that started before an edit", async () => {
    const hook = await seed("main", ["a", "b", "c"]);
    const stale = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings).mockReturnValueOnce(stale.promise);
    loadSettings("main");

    const held = deferred<SaveOutcome>();
    void saveField("main", "web", "list", ["b", "c"], () => held.promise);
    await act(async () => stale.resolve(plugins(["a", "b", "c"])));
    expect(listOf(hook)).toEqual(["b", "c"]);

    await act(async () => held.resolve("committed"));
    expect(listOf(hook)).toEqual(["b", "c"]);
  });

  it("drops a failure-triggered GET that lands after a later save settled", async () => {
    const hook = await seed("main", ["a", "b", "c"]);
    const stale = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings)
      .mockReturnValueOnce(stale.promise)
      .mockResolvedValue(plugins(["c"]));

    await act(async () => {
      await saveField("main", "web", "list", ["b", "c"], async () => "rejected");
    });
    await act(async () => {
      await saveField("main", "web", "list", ["c"], async () => "committed");
    });
    await act(async () => stale.resolve(plugins(["a", "b", "c"])));
    expect(listOf(hook)).toEqual(["c"]);
  });

  it("applies a GET only to the profile it was requested for", async () => {
    const mainHook = await seed("main", ["m"]);
    const slow = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings).mockReturnValueOnce(slow.promise);
    loadSettings("work");
    const workHook = renderHook(() => useProfileSettings("work"));
    expect(workHook.result.current.settings).toBeNull();

    await act(async () => slow.resolve(plugins(["w"])));
    expect(listOf(workHook)).toEqual(["w"]);
    expect(listOf(mainHook)).toEqual(["m"]);
  });
});
