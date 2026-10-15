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
  // Unscripted fetches (a retry after the test is over) hang instead of resolving to undefined.
  vi.mocked(fetchSettings)
    .mockReset()
    .mockReturnValue(new Promise(() => {}));
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

  it("keeps an unconfirmed edit over a fetch that predates its late commit", async () => {
    const hook = await seed("main", ["a", "b", "c"]);
    // The first fetch reads the old list: the write that dropped its connection has not committed yet.
    vi.mocked(fetchSettings).mockResolvedValueOnce(plugins(["a", "b", "c"]));
    await act(async () => {
      await saveField("main", "web", "list", ["b", "c"], async () => "uncertain");
    });
    await waitFor(() => expect(vi.mocked(fetchSettings).mock.calls.length).toBeGreaterThanOrEqual(2));
    expect(listOf(hook)).toEqual(["b", "c"]);

    // The next edit builds on the kept intent, not on the old list, so `a` stays removed.
    const next = (listOf(hook) ?? []).filter((x) => x !== "b");
    vi.mocked(fetchSettings).mockResolvedValue(plugins(["c"]));
    await act(async () => {
      await saveField("main", "web", "list", next, async () => "committed");
    });
    expect(listOf(hook)).toEqual(["c"]);

    // A later fetch showing the committed value settles the intent: no more reconciling.
    const calls = vi.mocked(fetchSettings).mock.calls.length;
    loadSettings("main");
    await waitFor(() => expect(fetchSettings).toHaveBeenCalledTimes(calls + 1));
    await act(async () => {
      await saveField("main", "web", "list", ["c"], async () => "committed");
    });
    expect(fetchSettings).toHaveBeenCalledTimes(calls + 1);
  });

  it("refetches when a GET was overtaken by a save that has since committed", async () => {
    const hook = await seed("main", ["a", "b", "c"]);
    // A mount-time GET still in flight when the user edits; the TUI changed the list meanwhile.
    const stale = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings)
      .mockReturnValueOnce(stale.promise)
      .mockResolvedValueOnce(plugins(["x", "b"]));
    loadSettings("main");
    await act(async () => {
      await saveField("main", "web", "flag", true, async () => "committed");
    });
    await act(async () => stale.resolve(plugins(["a", "b", "c"])));

    await waitFor(() => expect(listOf(hook)).toEqual(["x", "b"]));
  });

  it("resends an edit that never landed, then shows the saved value with a persistent error", async () => {
    const hook = await seed("main", ["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValue(plugins(["a", "b"]));
    const send = vi.fn(async (): Promise<SaveOutcome> => "uncertain");

    await act(async () => {
      await saveField("main", "web", "list", ["b"], send);
    });

    await waitFor(() => expect(hook.result.current.error).toMatch(/Could not save web\.list/));
    expect(send).toHaveBeenCalledTimes(3);
    expect(listOf(hook)).toEqual(["a", "b"]);
    // Nothing is left to reconcile: a further save does not fetch again.
    const calls = vi.mocked(fetchSettings).mock.calls.length;
    await act(async () => {
      await saveField("main", "web", "flag", true, async () => "committed");
    });
    expect(fetchSettings).toHaveBeenCalledTimes(calls);
  });

  it("keeps the could-not-confirm message when another field is edited meanwhile", async () => {
    const hook = await seed("main", ["a", "b"]);
    await act(async () => {
      await saveField("main", "web", "list", ["b"], async () => "uncertain");
    });
    const message = hook.result.current.error;
    expect(message).toMatch(/Could not confirm/);

    await act(async () => {
      await saveField("main", "web", "flag", true, async () => "committed");
    });
    expect(hook.result.current.error).toBe(message);
  });

  it("clears the could-not-confirm message once a fetch shows the edit", async () => {
    const hook = await seed("main", ["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValueOnce(plugins(["b"]));
    await act(async () => {
      await saveField("main", "web", "list", ["b"], async () => "uncertain");
    });
    await waitFor(() => expect(hook.result.current.error).toBeNull());
  });

  it("reports a failed refetch and resolves to false", async () => {
    const hook = await seed("main", ["a"]);
    vi.mocked(fetchSettings).mockResolvedValueOnce(null as never);
    let applied = true;
    await act(async () => {
      applied = await loadSettings("main");
    });
    expect(applied).toBe(false);
    expect(hook.result.current.error).toBe("Could not refresh settings, please reload");
    expect(listOf(hook)).toEqual(["a"]);
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
