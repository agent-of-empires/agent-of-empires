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

const pluginSettings = (list: string[]) => ({ plugins: { x: { settings: { list } } } });
const pluginList = (hook: { result: { current: SettingsView } }) =>
  (hook.result.current.settings?.plugins as { x: { settings: { list: string[] } } } | undefined)?.x.settings.list;

async function seedPluginProfile(profile: string, list: string[]) {
  vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(list));
  loadSettings(profile);
  const hook = renderHook(() => useProfileSettings(profile));
  await waitFor(() => expect(pluginList(hook)).toEqual(list));
  return hook;
}

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

  it("resends an edit that never landed, then leaves it unresolved on screen with a persistent error", async () => {
    const hook = await seed("main", ["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValue(plugins(["a", "b"]));
    const send = vi.fn(async (): Promise<SaveOutcome> => "uncertain");

    await act(async () => {
      await saveField("main", "web", "list", ["b"], send);
    });

    await waitFor(() => expect(hook.result.current.error).toMatch(/Could not confirm web\.list/));
    expect(send).toHaveBeenCalledTimes(3);
    // No fetch can prove a late commit is not coming, so the saved value does not replace the edit.
    expect(listOf(hook)).toEqual(["b"]);
  });

  it("builds the next edit on an unresolved one, so a late commit cannot restore a removal", async () => {
    const hook = await seed("main", ["a", "b", "c"]);
    // Every send reports an unknown outcome and every fetch still shows the old list.
    vi.mocked(fetchSettings).mockResolvedValue(plugins(["a", "b", "c"]));
    await act(async () => {
      await saveField("main", "web", "list", ["b", "c"], async () => "uncertain");
    });
    await waitFor(() => expect(hook.result.current.error).toMatch(/showing your value/));
    expect(listOf(hook)).toEqual(["b", "c"]);

    // The delayed commit lands, then the user removes `b`: the payload keeps `a` removed.
    const sent: unknown[] = [];
    const next = (listOf(hook) ?? []).filter((x) => x !== "b");
    await act(async () => {
      await saveField("main", "web", "list", next, async () => {
        vi.mocked(fetchSettings).mockResolvedValue(plugins(next as string[]));
        sent.push(next);
        return "committed";
      });
    });
    expect(sent).toEqual([["c"]]);
    expect(listOf(hook)).toEqual(["c"]);
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

  it("shows an accepted global plugin edit to another profile before its GET lands", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
    loadSettings("B");
    const b = renderHook(() => useProfileSettings("B"));
    await waitFor(() => expect(pluginList(b)).toEqual(["a", "b", "c"]));
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
    loadSettings("A");
    const a = renderHook(() => useProfileSettings("A"));
    await waitFor(() => expect(pluginList(a)).toEqual(["a", "b", "c"]));

    await act(async () => {
      await saveField("A", "plugin:x", "list", ["b", "c"], async () => "committed");
    });

    // Back on B with its GET held, removing `b` must not restore the removal of `a`.
    const held = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings)
      .mockReturnValueOnce(held.promise)
      .mockResolvedValue(pluginSettings(["c"]));
    loadSettings("B");
    expect(pluginList(b)).toEqual(["b", "c"]);
    const sendB = vi.fn(async (): Promise<SaveOutcome> => "committed");
    await act(async () => {
      await saveField(
        "B",
        "plugin:x",
        "list",
        (pluginList(b) ?? []).filter((x) => x !== "b"),
        sendB,
      );
    });
    expect(pluginList(b)).toEqual(["c"]);
    await act(async () => held.resolve(pluginSettings(["a", "b", "c"])));
    expect(pluginList(b)).toEqual(["c"]);
  });

  it("restores another profile's plugin value when the edit is rejected", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b"]));
    loadSettings("B");
    const b = renderHook(() => useProfileSettings("B"));
    await waitFor(() => expect(pluginList(b)).toEqual(["a", "b"]));
    await seed("A", ["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValue(pluginSettings(["a", "b"]));
    const held = deferred<SaveOutcome>();

    await act(async () => void saveField("A", "plugin:x", "list", ["b"], () => held.promise));
    expect(pluginList(b)).toEqual(["b"]);
    await act(async () => held.resolve("rejected"));
    expect(pluginList(b)).toEqual(["a", "b"]);
  });

  it("drops a GET another profile started during a global edit's save once that save settles", async () => {
    const seedPlugin = async (profile: string) => {
      vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
      loadSettings(profile);
      const hook = renderHook(() => useProfileSettings(profile));
      await waitFor(() => expect(pluginList(hook)).toEqual(["a", "b", "c"]));
      return hook;
    };
    const b = await seedPlugin("B");
    await seedPlugin("A");
    const held = deferred<SaveOutcome>();
    void saveField("A", "plugin:x", "list", ["b", "c"], () => held.promise);

    // B's GET starts after the mirror but reads the value from before A's save commits.
    const stale = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings)
      .mockReturnValueOnce(stale.promise)
      .mockResolvedValue(pluginSettings(["b", "c"]));
    loadSettings("B");
    await act(async () => held.resolve("committed"));
    await act(async () => stale.resolve(pluginSettings(["a", "b", "c"])));
    await waitFor(() => expect(pluginList(b)).toEqual(["b", "c"]));
  });

  it("keeps a mirrored global edit over a GET that lands before the origin save settles", async () => {
    const seedPlugin = async (profile: string) => {
      vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
      loadSettings(profile);
      const hook = renderHook(() => useProfileSettings(profile));
      await waitFor(() => expect(pluginList(hook)).toEqual(["a", "b", "c"]));
      return hook;
    };
    const b = await seedPlugin("B");
    await seedPlugin("A");
    const heldA = deferred<SaveOutcome>();
    void saveField("A", "plugin:x", "list", ["b", "c"], () => heldA.promise);

    // B's GET starts after the mirror and reads the old list before A's PATCH commits.
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
    await act(async () => {
      await loadSettings("B");
    });
    expect(pluginList(b)).toEqual(["b", "c"]);

    const sendB = vi.fn(async (): Promise<SaveOutcome> => "committed");
    vi.mocked(fetchSettings).mockResolvedValue(pluginSettings(["c"]));
    await act(async () => {
      await saveField(
        "B",
        "plugin:x",
        "list",
        (pluginList(b) ?? []).filter((x) => x !== "b"),
        sendB,
      );
    });
    expect(pluginList(b)).toEqual(["c"]);
    await act(async () => heldA.resolve("committed"));
    await waitFor(() => expect(pluginList(b)).toEqual(["c"]));
  });

  it("settles overlapping rejected global edits back to the server value, one mirror at a time", async () => {
    const b = await seedPluginProfile("B", ["a", "b", "c"]);
    await seedPluginProfile("A", ["a", "b", "c"]);
    vi.mocked(fetchSettings).mockResolvedValue(pluginSettings(["a", "b", "c"]));
    const first = deferred<SaveOutcome>();
    const second = deferred<SaveOutcome>();
    await act(async () => {
      void saveField("A", "plugin:x", "list", ["b", "c"], () => first.promise);
      void saveField("A", "plugin:x", "list", ["c"], () => second.promise);
    });
    expect(pluginList(b)).toEqual(["c"]);

    // The first rejection does not own B's mirror any more, so the second edit still shows.
    await act(async () => first.resolve("rejected"));
    await waitFor(() => expect(fetchSettings).toHaveBeenCalledTimes(3));
    expect(pluginList(b)).toEqual(["c"]);

    await act(async () => second.resolve("rejected"));
    await waitFor(() => expect(pluginList(b)).toEqual(["a", "b", "c"]));
  });

  it("keeps a newer origin's same-value mirror when an older origin's edit is rejected", async () => {
    const b = await seedPluginProfile("B", ["a", "b"]);
    await seedPluginProfile("A", ["a", "b"]);
    await seedPluginProfile("C", ["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValue(pluginSettings(["a", "b"]));
    const older = deferred<SaveOutcome>();
    const newer = deferred<SaveOutcome>();
    await act(async () => {
      void saveField("A", "plugin:x", "list", ["b"], () => older.promise);
      void saveField("C", "plugin:x", "list", ["b"], () => newer.promise);
    });

    await act(async () => older.resolve("rejected"));
    await waitFor(() => expect(fetchSettings.mock.calls.length).toBeGreaterThan(3));
    expect(pluginList(b)).toEqual(["b"]);
  });

  it("drops a GET already in flight for a profile when another profile's global edit is mirrored", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
    loadSettings("B");
    const b = renderHook(() => useProfileSettings("B"));
    await waitFor(() => expect(pluginList(b)).toEqual(["a", "b", "c"]));
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
    loadSettings("A");
    const a = renderHook(() => useProfileSettings("A"));
    await waitFor(() => expect(pluginList(a)).toEqual(["a", "b", "c"]));

    // B's GET is out, and B itself never saves, before A's edit is mirrored into it.
    const stale = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings)
      .mockReturnValueOnce(stale.promise)
      .mockResolvedValue(pluginSettings(["b", "c"]));
    loadSettings("B");
    const held = deferred<SaveOutcome>();
    void saveField("A", "plugin:x", "list", ["b", "c"], () => held.promise);
    const before = vi.mocked(fetchSettings).mock.calls.length;
    await act(async () => stale.resolve(pluginSettings(["a", "b", "c"])));

    await waitFor(() => expect(pluginList(b)).toEqual(["b", "c"]));
    expect(vi.mocked(fetchSettings).mock.calls.length).toBeGreaterThan(before);
  });

  it("refetches the other profiles once an uncertain global edit settles", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b"]));
    loadSettings("B");
    const b = renderHook(() => useProfileSettings("B"));
    await waitFor(() => expect(pluginList(b)).toEqual(["a", "b"]));
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b"]));
    loadSettings("A");
    const a = renderHook(() => useProfileSettings("A"));
    await waitFor(() => expect(pluginList(a)).toEqual(["a", "b"]));

    // The server never persisted the edit, so B must not keep building on the mirrored value.
    vi.mocked(fetchSettings).mockResolvedValue(pluginSettings(["a", "b"]));
    await act(async () => {
      await saveField("A", "plugin:x", "list", ["b"], async () => "uncertain");
    });
    await waitFor(() => expect(pluginList(b)).toEqual(["a", "b"]));
  });

  it("drops the first GET of a profile with no snapshot yet once a global edit settles", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b", "c"]));
    loadSettings("A");
    const a = renderHook(() => useProfileSettings("A"));
    await waitFor(() => expect(pluginList(a)).toEqual(["a", "b", "c"]));

    // B has no snapshot to mirror into, and its first GET reads the value from before the commit.
    const stale = deferred<Record<string, unknown>>();
    vi.mocked(fetchSettings)
      .mockReturnValueOnce(stale.promise)
      .mockResolvedValue(pluginSettings(["b", "c"]));
    loadSettings("B");
    const b = renderHook(() => useProfileSettings("B"));
    await act(async () => {
      await saveField("A", "plugin:x", "list", ["b", "c"], async () => "committed");
    });
    await act(async () => stale.resolve(pluginSettings(["a", "b", "c"])));
    await waitFor(() => expect(pluginList(b)).toEqual(["b", "c"]));
  });

  it("keeps another profile's own save of the same value when the mirrored edit is rejected", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b"]));
    loadSettings("B");
    const b = renderHook(() => useProfileSettings("B"));
    await waitFor(() => expect(pluginList(b)).toEqual(["a", "b"]));
    vi.mocked(fetchSettings).mockResolvedValueOnce(pluginSettings(["a", "b"]));
    loadSettings("A");
    const a = renderHook(() => useProfileSettings("A"));
    await waitFor(() => expect(pluginList(a)).toEqual(["a", "b"]));
    vi.mocked(fetchSettings).mockResolvedValue(pluginSettings(["b"]));
    const held = deferred<SaveOutcome>();

    await act(async () => void saveField("A", "plugin:x", "list", ["b"], () => held.promise));
    await act(async () => {
      await saveField("B", "plugin:x", "list", ["b"], async () => "committed");
    });
    await act(async () => held.resolve("rejected"));
    expect(pluginList(b)).toEqual(["b"]);
  });

  it("clears the refresh warning once a refetch succeeds", async () => {
    const hook = await seed("main", ["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValueOnce(null as never);
    await act(async () => {
      await loadSettings("main");
    });
    expect(hook.result.current.error).toBe("Could not refresh settings, please reload");
    vi.mocked(fetchSettings).mockResolvedValueOnce(plugins(["a", "b"]));
    await act(async () => {
      await loadSettings("main");
    });
    expect(hook.result.current.error).toBeNull();
  });
});
