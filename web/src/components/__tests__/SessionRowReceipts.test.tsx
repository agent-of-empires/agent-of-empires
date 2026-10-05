// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useEffect } from "react";
import { useSessions } from "../../hooks/useSessions";
import { setServerDown } from "../../lib/connectionState";
import { EMPTY_OPTIMISTIC } from "../../lib/sidebarOptimistic";
import { toastBus } from "../../lib/toastBus";
import { SessionRow } from "../sidebar/SessionRow";
import { makeSession, makeWorkspace } from "./fixtures";

let canonical: ReturnType<typeof useSessions>;
function Harness({ batch = false }: { batch?: boolean }) {
  const current = useSessions();
  useEffect(() => {
    canonical = current;
  });
  const workspaces = batch
    ? [makeWorkspace("batch", current.sessions)]
    : current.sessions.map((session) => makeWorkspace(session.id, [session]));
  return (
    <>
      {workspaces.map((workspace) => (
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
    </>
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

describe("SessionRow canonical settings receipts", () => {
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
