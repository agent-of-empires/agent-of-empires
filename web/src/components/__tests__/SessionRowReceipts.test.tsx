// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useEffect } from "react";
import { useSessions } from "../../hooks/useSessions";
import { useSettingsCommands } from "../../hooks/useSettingsCommands";
import { setServerDown } from "../../lib/connectionState";
import { EMPTY_OPTIMISTIC } from "../../lib/sidebarOptimistic";
import { toastBus } from "../../lib/toastBus";
import { SessionRow } from "../sidebar/SessionRow";
import { makeSession, makeWorkspace } from "./fixtures";
import { SessionActionProvider } from "../sidebar/SessionActionProvider";

let canonical: ReturnType<typeof useSessions>;
function Harness({
  batch = false,
  showRows = true,
  readOnly = false,
}: {
  batch?: boolean;
  showRows?: boolean;
  readOnly?: boolean;
}) {
  const current = useSessions();
  useEffect(() => {
    canonical = current;
  });
  const workspaces = batch
    ? [makeWorkspace("batch", current.sessions)]
    : current.sessions.map((session) => makeWorkspace(session.id, [session]));
  return (
    <SessionActionProvider
      sessions={current.sessions}
      runtimeEpoch={current.runtimeEpoch}
      available={!current.error}
      readOnly={readOnly}
      observedById={current.observedById}
      onSessionMutation={current.applySessionMutation}
    >
      {showRows &&
        workspaces.map((workspace) => (
          <SessionRow
            key={workspace.id}
            workspace={workspace}
            observedById={current.observedById}
            runtimeEpoch={current.runtimeEpoch}
            onSessionMutation={current.applySessionMutation}
            isActive={false}
            isSelected={false}
            onActivate={() => {}}
            optimistic={EMPTY_OPTIMISTIC}
            onPinToggle={() => {}}
            onArchiveToggle={() => {}}
            onSnooze={() => {}}
            onUnreadToggle={() => {}}
            bulkApi={{ prepareScope: () => ({ kind: "single" }), pin: () => {}, archive: () => {}, snooze: () => {} }}
          />
        ))}
    </SessionActionProvider>
  );
}
const reply = (body: unknown, revision: bigint, epoch = "boot") =>
  new Response(JSON.stringify(body), {
    headers: {
      "content-type": "application/json",
      "aoe-runtime-epoch": epoch,
      "aoe-runtime-revision": revision.toString(),
    },
  });
async function tick(ms = 0) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}
function menu(index = 0) {
  fireEvent.mouseDown(document.body);
  fireEvent.contextMenu(screen.getAllByTestId("sidebar-session-row")[index]!);
}
async function choose(id: string) {
  await act(async () => fireEvent.click(screen.getByTestId(id)));
}
const picked = (id: string) => screen.getByTestId(id).getAttribute("aria-pressed");

beforeEach(() => {
  vi.useFakeTimers();
  setServerDown(false);
});
afterEach(() => {
  cleanup();
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  setServerDown(false);
  toastBus.handler = null;
});

const SETTING_CASES = [
  {
    kind: "color",
    pick: "sidebar-context-menu-color-red",
    reset: "sidebar-context-menu-color-clear",
    later: "sidebar-context-menu-color-amber",
    selected: (session: ReturnType<typeof makeSession>) => ({ ...session, color: "red" }),
    newer: (session: ReturnType<typeof makeSession>) => ({ ...session, color: "amber" }),
  },
  {
    kind: "notifications",
    pick: "sidebar-context-menu-notify-all",
    reset: "sidebar-context-menu-notify-default",
    later: "sidebar-context-menu-notify-off",
    selected: (session: ReturnType<typeof makeSession>) => ({
      ...session,
      notify_on_waiting: true,
      notify_on_idle: true,
      notify_on_error: true,
    }),
    newer: (session: ReturnType<typeof makeSession>) => ({
      ...session,
      notify_on_waiting: false,
      notify_on_idle: false,
      notify_on_error: false,
    }),
  },
] as const;

function SettingsCommandsConsumer({ open }: { open: boolean }) {
  useSettingsCommands({ open, readOnly: false, onOpenSettingsTab: () => {} });
  return null;
}

describe.each(SETTING_CASES)("accepted $kind settings across authority availability changes", (setting) => {
  it.each(["committed", "uncertain"] as const)(
    "retains the accepted result after list and settings reads fail (%s)",
    async (outcome) => {
      const A = makeSession({ id: "A", color: null });
      const B = makeSession({ id: "B", color: null });
      const greenB = { ...B, color: "green" };
      const save = Promise.withResolvers<Response>();
      const entered = Promise.withResolvers<void>();
      const failedSettingsEntered = Promise.withResolvers<void>();
      let failPoll = false;
      let revision = 10n;
      let snapshot = { sessions: [A, B], workspace_ordering: ["A", "B"] };
      const writes: RequestInit[] = [];
      const error = vi.fn();
      toastBus.handler = { error, info: vi.fn(), push: vi.fn(), openLink: vi.fn() };
      vi.stubGlobal(
        "fetch",
        vi.fn(async (url: string, init?: RequestInit) => {
          if (url === "/api/sessions") {
            return failPoll ? new Response("", { status: 503 }) : reply(snapshot, revision);
          }
          if (url === "/api/settings/schema")
            return reply(
              [
                {
                  section: "session",
                  field: "show_session_colors",
                  category: "session",
                  label: "Session colors",
                  description: "",
                  widget: { kind: "toggle" },
                  web_write: { policy: "allow" },
                  profile_overridable: false,
                  validation: { rule: "none" },
                  advanced: false,
                },
              ],
              revision,
            );
          if (url === "/api/profiles") return reply([{ name: "default", is_default: true }], revision);
          if (url === "/api/settings") {
            if (failPoll) {
              failedSettingsEntered.resolve();
              return new Response("", { status: 503 });
            }
            return reply({ session: { show_session_colors: true } }, revision);
          }
          if (url === "/api/sessions/B/color") return reply(greenB, 12n);
          if (url === `/api/sessions/A/${setting.kind}`) {
            writes.push(init!);
            entered.resolve();
            return save.promise;
          }
          throw new Error("Unexpected request " + url);
        }),
      );
      const content = (open: boolean) => (
        <>
          <Harness />
          <SettingsCommandsConsumer open={open} />
        </>
      );
      const view = render(content(true));
      await tick();
      menu();
      await choose(setting.pick);
      await entered.promise;
      expect(picked(setting.pick)).toBe("true");
      menu(1);
      await choose("sidebar-context-menu-color-green");
      expect(canonical.observedById.B?.revision).toBe(12n);
      expect(canonical.observedById.A?.revision).toBe(10n);
      expect(canonical.sessions[0]).toEqual(A);
      menu();
      expect(picked(setting.pick)).toBe("true");
      view.rerender(content(false));
      failPoll = true;
      view.rerender(content(true));
      await tick(3000);
      await failedSettingsEntered.promise;
      expect(canonical.error).toBe(true);
      expect(picked(setting.pick)).toBe("true");
      await act(async () =>
        save.resolve(outcome === "committed" ? reply(setting.selected(A), 11n) : new Response("", { status: 503 })),
      );
      expect(writes).toHaveLength(1);
      expect(canonical.observedById.B?.revision).toBe(12n);
      if (outcome === "committed") {
        expect(canonical.sessions[0]).toEqual(setting.selected(A));
        expect(canonical.observedById.A?.revision).toBe(11n);
        expect(picked(setting.pick)).toBe("true");
        expect(error).not.toHaveBeenCalled();
      } else {
        expect(canonical.sessions[0]).toEqual(A);
        expect(canonical.observedById.A?.revision).toBe(10n);
        expect(picked(setting.reset)).toBe("true");
        expect(error).toHaveBeenCalledTimes(1);
      }
      failPoll = false;
      revision = 13n;
      snapshot = { sessions: [setting.newer(A), greenB], workspace_ordering: ["A", "B"] };
      await tick(3000);
      expect(canonical.error).toBe(false);
      expect(canonical.sessions[0]).toEqual(setting.newer(A));
      expect(canonical.observedById.A?.revision).toBe(13n);
      expect(picked(setting.later)).toBe("true");
      expect(picked(setting.pick)).toBe("false");
    },
  );

  it("dispatches an accepted queued reset during a poll outage", async () => {
    const A = makeSession({ id: "A", color: null });
    const first = Promise.withResolvers<Response>();
    const second = Promise.withResolvers<Response>();
    const entered = Promise.withResolvers<void>();
    const secondEntered = Promise.withResolvers<void>();
    let failPoll = false;
    const writes: RequestInit[] = [];
    const error = vi.fn();
    toastBus.handler = { error, info: vi.fn(), push: vi.fn(), openLink: vi.fn() };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        if (url === "/api/sessions") {
          return failPoll ? new Response("", { status: 503 }) : reply({ sessions: [A], workspace_ordering: [] }, 10n);
        }
        if (url !== `/api/sessions/A/${setting.kind}`) throw new Error("Unexpected request " + url);
        writes.push(init!);
        if (writes.length === 1) {
          entered.resolve();
          return first.promise;
        }
        secondEntered.resolve();
        return second.promise;
      }),
    );
    render(<Harness />);
    await tick();
    menu();
    await choose(setting.pick);
    await entered.promise;
    await choose(setting.reset);
    expect(picked(setting.reset)).toBe("true");
    failPoll = true;
    await tick(3000);
    expect(canonical.error).toBe(true);
    await act(async () => first.resolve(reply(setting.selected(A), 11n)));
    await secondEntered.promise;
    expect(canonical.sessions[0]).toEqual(setting.selected(A));
    expect(canonical.observedById.A?.revision).toBe(11n);
    expect(picked(setting.reset)).toBe("true");
    await act(async () => second.resolve(reply(A, 12n)));
    expect(writes).toHaveLength(2);
    expect(canonical.sessions).toEqual([A]);
    expect(canonical.observedById.A?.revision).toBe(12n);
    expect(picked(setting.reset)).toBe("true");
    expect(error).not.toHaveBeenCalled();
  });

  it.each([false, true])(
    "adopts a dispatched receipt but refuses any queued write after read-only admission changes (queued=%s)",
    async (queued) => {
      const A = makeSession({ id: "A", color: null });
      const held = Promise.withResolvers<Response>();
      const entered = Promise.withResolvers<void>();
      const writes: RequestInit[] = [];
      const error = vi.fn();
      toastBus.handler = { error, info: vi.fn(), push: vi.fn(), openLink: vi.fn() };
      vi.stubGlobal(
        "fetch",
        vi.fn(async (url: string, init?: RequestInit) => {
          if (url === "/api/sessions") return reply({ sessions: [A], workspace_ordering: [] }, 10n);
          if (url !== `/api/sessions/A/${setting.kind}`) throw new Error("Unexpected request " + url);
          writes.push(init!);
          entered.resolve();
          return held.promise;
        }),
      );
      const view = render(<Harness />);
      await tick();
      menu();
      await choose(setting.pick);
      await entered.promise;
      if (queued) await choose(setting.reset);
      expect(picked(queued ? setting.reset : setting.pick)).toBe("true");
      view.rerender(<Harness readOnly />);
      await act(async () => held.resolve(reply(setting.selected(A), 11n)));
      expect(writes).toHaveLength(1);
      expect(canonical.sessions[0]).toEqual(setting.selected(A));
      expect(canonical.observedById.A?.revision).toBe(11n);
      expect(picked(setting.pick)).toBe("true");
      expect(picked(setting.reset)).toBe("false");
      if (queued) expect(error).toHaveBeenCalledTimes(1);
      else expect(error).not.toHaveBeenCalled();
      view.rerender(<Harness />);
      await tick();
      expect(writes).toHaveLength(1);
      expect(picked(setting.pick)).toBe("true");
    },
  );

  it.each(["unavailable", "readOnly"] as const)(
    "rejects a new pick without replacing the accepted selection when %s",
    async (admission) => {
      const A = makeSession({ id: "A", color: null });
      const held = Promise.withResolvers<Response>();
      const entered = Promise.withResolvers<void>();
      let failPoll = false;
      let writes = 0;
      const error = vi.fn();
      toastBus.handler = { error, info: vi.fn(), push: vi.fn(), openLink: vi.fn() };
      vi.stubGlobal(
        "fetch",
        vi.fn(async (url: string) => {
          if (url === "/api/sessions")
            return failPoll ? new Response("", { status: 503 }) : reply({ sessions: [A], workspace_ordering: [] }, 10n);
          if (url !== `/api/sessions/A/${setting.kind}`) throw new Error("Unexpected request " + url);
          writes++;
          entered.resolve();
          return held.promise;
        }),
      );
      const view = render(<Harness />);
      await tick();
      menu();
      await choose(setting.pick);
      await entered.promise;
      if (admission === "unavailable") {
        failPoll = true;
        await tick(3000);
        expect(canonical.error).toBe(true);
      } else view.rerender(<Harness readOnly />);
      await choose(setting.later);
      expect(writes).toBe(1);
      expect(canonical.sessions[0]).toEqual(A);
      expect(canonical.observedById.A?.revision).toBe(10n);
      expect(picked(setting.pick)).toBe("true");
      expect(picked(setting.later)).toBe("false");
      expect(error).toHaveBeenCalledTimes(1);
      error.mockClear();
      await act(async () => held.resolve(reply(setting.selected(A), 11n)));
      expect(canonical.sessions[0]).toEqual(setting.selected(A));
      expect(canonical.observedById.A?.revision).toBe(11n);
      expect(picked(setting.pick)).toBe("true");
      expect(error).not.toHaveBeenCalled();
      failPoll = false;
      view.rerender(<Harness />);
      await tick();
      expect(writes).toBe(1);
      expect(picked(setting.pick)).toBe("true");
      expect(picked(setting.later)).toBe("false");
    },
  );
});

describe("SessionRow canonical settings receipts", () => {
  it.each(["color", "notifications"] as const)(
    "finishes the accepted %s reset after rows visually unmount",
    async (setting) => {
      const A = makeSession({ id: "A", color: null });
      const first = Promise.withResolvers<Response>();
      const second = Promise.withResolvers<Response>();
      const firstEntered = Promise.withResolvers<void>();
      const secondEntered = Promise.withResolvers<RequestInit>();
      let calls = 0;
      const selected =
        setting === "color"
          ? { ...A, color: "red" }
          : { ...A, notify_on_waiting: true, notify_on_idle: true, notify_on_error: true };
      vi.stubGlobal(
        "fetch",
        vi.fn(async (url: string, init?: RequestInit) => {
          if (url === "/api/sessions") return reply({ sessions: [A], workspace_ordering: [] }, 10n);
          if (++calls === 1) {
            firstEntered.resolve();
            return first.promise;
          }
          secondEntered.resolve(init!);
          return second.promise;
        }),
      );
      const view = render(<Harness />);
      await tick();
      menu();
      await choose(setting === "color" ? "sidebar-context-menu-color-red" : "sidebar-context-menu-notify-all");
      await firstEntered.promise;
      await choose(setting === "color" ? "sidebar-context-menu-color-clear" : "sidebar-context-menu-notify-default");
      view.rerender(<Harness showRows={false} />);
      expect(screen.queryByTestId("sidebar-session-row")).toBeNull();
      await act(async () => first.resolve(reply(selected, 11n)));
      const request = await secondEntered.promise;
      expect(JSON.parse(request.body as string)).toEqual(
        setting === "color"
          ? { color: null }
          : { notify_on_waiting: null, notify_on_idle: null, notify_on_error: null },
      );
      expect(new Headers(request.headers).get("aoe-runtime-epoch")).toBe("boot");
      await act(async () => second.resolve(reply(A, 12n)));
      view.rerender(<Harness />);
      menu();
      expect(
        picked(setting === "color" ? "sidebar-context-menu-color-clear" : "sidebar-context-menu-notify-default"),
      ).toBe("true");
      expect(canonical.sessions).toEqual([A]);
      expect(canonical.observedById.A?.revision).toBe(12n);
    },
  );

  it.each(["epoch", "removed", "birth", "profile", "removed-and-restored"] as const)(
    "does not replay an accepted old setting after actual %s invalidation",
    async (change) => {
      const A = makeSession({ id: "A", color: null });
      const held = Promise.withResolvers<Response>();
      let snapshot = { sessions: [A], workspace_ordering: [] };
      let epoch = "boot";
      let calls = 0;
      const error = vi.fn();
      toastBus.handler = { error, info: vi.fn(), push: vi.fn(), openLink: vi.fn() };
      vi.stubGlobal(
        "fetch",
        vi.fn(async (url: string) => {
          if (url === "/api/sessions") return reply(snapshot, 20n, epoch);
          calls++;
          return held.promise;
        }),
      );
      render(<Harness />);
      await tick();
      menu();
      await choose("sidebar-context-menu-color-red");
      expect(calls).toBe(1);
      await choose("sidebar-context-menu-color-clear");
      if (change === "epoch") epoch = "replacement";
      if (change === "removed" || change === "removed-and-restored")
        snapshot = { sessions: [], workspace_ordering: [] };
      if (change === "birth")
        snapshot = { sessions: [{ ...A, created_at: "2026-01-01T00:00:00Z" }], workspace_ordering: [] };
      if (change === "profile") snapshot = { sessions: [{ ...A, profile: "replacement" }], workspace_ordering: [] };
      await tick(3000);
      expect(canonical.sessions).toEqual(snapshot.sessions);
      if (change === "removed-and-restored") {
        snapshot = { sessions: [A], workspace_ordering: [] };
        await tick(3000);
      }
      await act(async () => held.resolve(reply({ ...A, color: "red" }, 21n)));
      expect(calls).toBe(1);
      expect(canonical.sessions).toEqual(snapshot.sessions);
      expect(error).toHaveBeenCalledWith(expect.stringContaining("unconfirmed"));
    },
  );

  it("keeps the newer accepted pick when an older save fails after visual unmount", async () => {
    const A = makeSession({ id: "A", color: null });
    const held = Promise.withResolvers<Response>();
    let calls = 0;
    const error = vi.fn();
    toastBus.handler = { error, info: vi.fn(), push: vi.fn(), openLink: vi.fn() };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/api/sessions") return reply({ sessions: [A], workspace_ordering: [] }, 10n);
        return ++calls === 1 ? held.promise : reply({ ...A, color: "amber" }, 12n);
      }),
    );
    const view = render(<Harness />);
    await tick();
    menu();
    await choose("sidebar-context-menu-color-red");
    await choose("sidebar-context-menu-color-amber");
    view.rerender(<Harness showRows={false} />);
    await act(async () => held.resolve(new Response("", { status: 503 })));
    expect(canonical.sessions[0]?.color).toBe("amber");
    expect(error).not.toHaveBeenCalled();
    view.rerender(<Harness />);
    menu();
    expect(picked("sidebar-context-menu-color-amber")).toBe("true");
  });

  it.each(["color", "notifications"])(
    "keeps cancelled %s through an unrelated B12 ACK and stale GET11, then accepts writer13",
    async (setting) => {
      const first = Promise.withResolvers<Response>();
      const cancel = Promise.withResolvers<Response>();
      let callsA = 0;
      const initialA = makeSession({ id: "A", color: null });
      const B = makeSession({ id: "B", color: null });
      const selectedA =
        setting === "color"
          ? { ...initialA, color: "red" }
          : { ...initialA, notify_on_waiting: true, notify_on_idle: true, notify_on_error: true };
      let snapshot = { sessions: [initialA, B], workspace_ordering: ["A", "B"] };
      let revision = 9n;
      vi.stubGlobal(
        "fetch",
        vi.fn(async (url: string) => {
          if (url === "/api/sessions") return reply(snapshot, revision);
          if (url === "/api/sessions/A/" + setting) return ++callsA === 1 ? first.promise : cancel.promise;
          if (url === "/api/sessions/B/color") return reply({ ...B, color: "green" }, 12n);
          throw new Error("Unexpected request " + url);
        }),
      );
      render(<Harness />);
      await tick();
      menu();
      const earlier = setting === "color" ? "sidebar-context-menu-color-red" : "sidebar-context-menu-notify-all";
      const latest = setting === "color" ? "sidebar-context-menu-color-clear" : "sidebar-context-menu-notify-default";
      await choose(earlier);
      await choose(latest);
      expect(callsA).toBe(1);
      snapshot = { sessions: [selectedA, B], workspace_ordering: ["A", "B"] };
      revision = 10n;
      await tick(3000);
      expect(picked(latest)).toBe("true");
      menu(1);
      await choose("sidebar-context-menu-color-green");
      expect(canonical.observedById.B?.revision).toBe(12n);
      expect(canonical.observedById.A?.revision).toBe(10n);
      menu();
      expect(picked(latest)).toBe("true");
      await act(async () => first.resolve(reply(selectedA, 10n)));
      expect(callsA).toBe(2);
      expect(picked(latest)).toBe("true");
      await act(async () => cancel.resolve(reply(initialA, 11n)));
      expect(picked(latest)).toBe("true");
      expect(canonical.observedById.A?.revision).toBe(11n);
      expect(canonical.sessions[0]).toEqual(initialA);
      if (setting === "color")
        expect(
          screen.queryAllByTestId("sidebar-session-color-dot").map((dot) => dot.getAttribute("data-color")),
        ).toEqual(["green"]);
      revision = 11n;
      await tick(3000);
      expect(picked(latest)).toBe("true");
      expect(canonical.observedById.A?.revision).toBe(11n);
      revision = 13n;
      await tick(3000);
      expect(picked(earlier)).toBe("true");
      expect(picked(latest)).toBe("false");
    },
  );

  it.each([false, true])(
    "does not resurrect a surrendered overlay when canonical color returns (repick %s)",
    async (repick) => {
      const A = makeSession({ id: "A", color: null });
      const held = Promise.withResolvers<Response>();
      let calls = 0;
      vi.stubGlobal(
        "fetch",
        vi.fn(async (url: string) => {
          if (url === "/api/sessions") return reply({ sessions: [A], workspace_ordering: [] }, 9n);
          return ++calls === 1 ? held.promise : reply({ ...A, color: "red" }, 13n);
        }),
      );
      render(<Harness />);
      await tick();
      menu();
      await choose("sidebar-context-menu-color-red");
      act(() =>
        canonical.applySessionMutation({
          session: { ...A, color: "green" },
          cursor: { epoch: "boot", revision: 11n },
        }),
      );
      expect(picked("sidebar-context-menu-color-green")).toBe("true");
      act(() =>
        canonical.applySessionMutation({
          session: A,
          cursor: { epoch: "boot", revision: 12n },
        }),
      );
      expect(picked("sidebar-context-menu-color-clear")).toBe("true");
      if (repick) await choose("sidebar-context-menu-color-red");
      await act(async () => held.resolve(reply({ ...A, color: "red" }, 10n)));
      expect(calls).toBe(repick ? 2 : 1);
      expect(picked(repick ? "sidebar-context-menu-color-red" : "sidebar-context-menu-color-clear")).toBe("true");
    },
  );

  it("releases an unchanged cancellation without another GET and respects a newer same-id row before its ACK", async () => {
    const first = Promise.withResolvers<Response>();
    const cancel = Promise.withResolvers<Response>();
    const A = makeSession({ id: "A", color: null });
    let calls = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) =>
        url === "/api/sessions"
          ? reply({ sessions: [A], workspace_ordering: [] }, 10n)
          : ++calls === 1
            ? first.promise
            : cancel.promise,
      ),
    );
    render(<Harness />);
    await tick();
    menu();
    await choose("sidebar-context-menu-color-red");
    await choose("sidebar-context-menu-color-clear");
    await act(async () => first.resolve(reply({ ...A, color: "red" }, 11n)));
    await act(async () => cancel.resolve(reply(A, 12n)));
    expect(picked("sidebar-context-menu-color-clear")).toBe("true");
    act(() =>
      canonical.applySessionMutation({ session: { ...A, color: "red" }, cursor: { epoch: "boot", revision: 13n } }),
    );
    expect(picked("sidebar-context-menu-color-red")).toBe("true");
    const older = Promise.withResolvers<Response>();
    vi.mocked(fetch).mockImplementationOnce(() => older.promise);
    await choose("sidebar-context-menu-color-clear");
    act(() =>
      canonical.applySessionMutation({ session: { ...A, color: "amber" }, cursor: { epoch: "boot", revision: 15n } }),
    );
    await act(async () => older.resolve(reply(A, 14n)));
    expect(picked("sidebar-context-menu-color-amber")).toBe("true");
    expect(canonical.sessions[0]!.color).toBe("amber");
    expect(canonical.observedById.A?.revision).toBe(15n);
  });

  it("applies every successful multi-session color response and reports partial failure without inventing an ACK", async () => {
    const A = makeSession({ id: "A", color: "red" });
    const B = makeSession({ id: "B", color: "red" });
    const error = vi.fn();
    toastBus.handler = { error, info: vi.fn(), push: vi.fn(), openLink: vi.fn() };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/api/sessions") return reply({ sessions: [A, B], workspace_ordering: [] }, 10n);
        if (url === "/api/sessions/A/color") return reply({ ...A, color: null }, 11n);
        return new Response("", { status: 503 });
      }),
    );
    render(<Harness batch />);
    await tick();
    menu();
    await choose("sidebar-context-menu-color-clear");
    expect(error).toHaveBeenCalledOnce();
    expect(canonical.sessions.map((row) => row.color)).toEqual([null, "red"]);
    expect(canonical.observedById.A?.revision).toBe(11n);
    expect(canonical.observedById.B?.revision).toBe(10n);
    expect(picked("sidebar-context-menu-color-red")).toBe("true");
  });

  it("holds a captured two-id batch until both receipts are installed, even when B ACK arrives first", async () => {
    const A = makeSession({ id: "A", color: "red" });
    const B = makeSession({ id: "B", color: "red" });
    const ackA = Promise.withResolvers<Response>();
    const ackB = Promise.withResolvers<Response>();
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/api/sessions") return reply({ sessions: [A, B], workspace_ordering: [] }, 10n);
        return url.includes("/A/") ? ackA.promise : ackB.promise;
      }),
    );
    render(<Harness batch />);
    await tick();
    menu();
    await choose("sidebar-context-menu-color-clear");
    await act(async () => ackB.resolve(reply({ ...B, color: null }, 12n)));
    expect(canonical.observedById.A?.revision).toBe(10n);
    expect(canonical.observedById.B?.revision).toBe(12n);
    expect(picked("sidebar-context-menu-color-clear")).toBe("true");
    await act(async () => ackA.resolve(reply({ ...A, color: null }, 11n)));
    expect(canonical.observedById.A?.revision).toBe(11n);
    act(() => {
      canonical.applySessionMutation({ session: { ...A, color: "green" }, cursor: { epoch: "boot", revision: 13n } });
      canonical.applySessionMutation({ session: { ...B, color: "green" }, cursor: { epoch: "boot", revision: 14n } });
    });
    expect(picked("sidebar-context-menu-color-green")).toBe("true");
  });
});
