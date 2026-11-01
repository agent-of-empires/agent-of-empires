// @vitest-environment jsdom

import { useState, type ComponentProps } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { makeSession, makeWorkspace, stubFetch } from "../../__tests__/fixtures";
import { PluginSessionMenu as MenuContent, type SessionMenuRequest } from "../PluginSessionMenu";
import { useSessions } from "../../../hooks/useSessions";
import type { SessionMutation } from "../../../lib/api";
import { SessionActionProvider } from "../../sidebar/SessionActionProvider";

function PluginSessionMenu(props: ComponentProps<typeof MenuContent>) {
  return (
    <SessionActionProvider
      sessions={props.workspaces.flatMap((ws) => ws.sessions)}
      runtimeEpoch={props.runtimeEpoch}
      observedById={props.observedById}
      onSessionMutation={props.handlers.onSessionMutation}
      readOnly={props.readOnly}
    >
      <MenuContent {...props} />
    </SessionActionProvider>
  );
}

const workspaces = [makeWorkspace("w1", [makeSession({ id: "s1", title: "Ethiopians" })])];

const handlers = { onStop: vi.fn(), onDelete: vi.fn(), onSessionMutation: vi.fn() };
const observation = {
  observedById: { s1: { epoch: "native-test", revision: 1n }, s2: { epoch: "native-test", revision: 1n } },
  runtimeEpoch: "native-test",
};

function CanonicalMenu({ request, defer = false }: { request: SessionMenuRequest; defer?: boolean }) {
  const sessions = useSessions();
  const [receipt, setReceipt] = useState<SessionMutation | null>(null);
  const cursor = sessions.observedById.s1;
  return (
    <>
      <output data-testid="canonical-title">{sessions.sessions[0]?.title ?? "waiting"}</output>
      <output data-testid="canonical-cursor">{cursor ? `${cursor.epoch}:${cursor.revision}` : "unobserved"}</output>
      {defer && (
        <button
          onClick={() => {
            if (receipt) sessions.applySessionMutation(receipt);
          }}
        >
          Install canonical receipt
        </button>
      )}
      {sessions.loaded && (
        <PluginSessionMenu
          request={request}
          workspaces={[makeWorkspace("w1", sessions.sessions)]}
          readOnly={false}
          handlers={{
            ...handlers,
            onSessionMutation: (mutation) => {
              handlers.onSessionMutation(mutation);
              if (defer) setReceipt(mutation);
              else sessions.applySessionMutation(mutation);
            },
          }}
          observedById={sessions.observedById}
          runtimeEpoch={sessions.runtimeEpoch}
        />
      )}
    </>
  );
}

function open(over: Partial<SessionMenuRequest> = {}, readOnly = false) {
  const onClose = vi.fn();
  const request: SessionMenuRequest = {
    key: 1,
    sessionId: "s1",
    x: 40,
    y: 50,
    items: [],
    source: "Board",
    onClose,
    ...over,
  };
  render(
    <PluginSessionMenu
      request={request}
      workspaces={workspaces}
      readOnly={readOnly}
      handlers={handlers}
      {...observation}
    />,
  );
  return onClose;
}

const backdrop = () => screen.getByTestId("plugin-session-menu-backdrop");
const arm = () => act(() => vi.advanceTimersByTime(300));

let fetchSpy: ReturnType<typeof stubFetch>;
beforeEach(() => {
  vi.useFakeTimers();
  fetchSpy = stubFetch();
  handlers.onStop.mockReset();
  handlers.onDelete.mockReset();
  handlers.onSessionMutation.mockReset();
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("PluginSessionMenu", () => {
  it("shields the menu until armed, then a page item resolves with its id", () => {
    const onClose = open({ items: [{ id: "move:done", label: "Move to Done" }] });
    expect(screen.getByTestId("sidebar-context-menu")).toBeTruthy();
    // The page's items sit under its title, apart from the dashboard's actions, and above
    // Move to Trash… so the destructive item stays last.
    expect(screen.getByText("Board")).toBeTruthy();
    const pageItem = screen.getByRole("button", { name: "Move to Done" });
    const trash = screen.getByTestId("sidebar-context-menu-delete");
    expect(pageItem.compareDocumentPosition(trash) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    // One rule between the page's tier and Trash, not two.
    expect(trash.previousElementSibling?.className).toContain("border-t");
    expect(trash.previousElementSibling?.previousElementSibling?.className ?? "").not.toContain("border-t");
    expect(backdrop().className).toContain("z-[60]");
    expect(backdrop().dataset.armed).toBeUndefined();
    fireEvent.mouseDown(backdrop());
    fireEvent.contextMenu(backdrop());
    expect(screen.getByTestId("sidebar-context-menu")).toBeTruthy();

    arm();
    expect(backdrop().className).toContain("z-40");
    expect(backdrop().dataset.armed).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Move to Done" }));
    expect(screen.queryByTestId("sidebar-context-menu")).toBeNull();
    expect(onClose).toHaveBeenCalledExactlyOnceWith("move:done");
  });

  it("an armed backdrop click closes with nothing picked", () => {
    const onClose = open();
    arm();
    fireEvent.mouseDown(backdrop());
    expect(screen.queryByTestId("sidebar-context-menu")).toBeNull();
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
  });

  it("resolves null at once for a session the dashboard does not know", () => {
    const onClose = open({ sessionId: "missing" });
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
    expect(screen.queryByTestId("sidebar-context-menu")).toBeNull();
  });

  it("completes rename only after the real session consumer installs its canonical receipt", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          sessions: workspaces[0]!.sessions,
          workspace_ordering: [],
        }),
        { headers: { "aoe-runtime-epoch": "native-test", "aoe-runtime-revision": "1" } },
      ),
    );
    const onClose = vi.fn();
    const request: SessionMenuRequest = { key: 1, sessionId: "s1", x: 40, y: 50, items: [], source: "Board", onClose };
    await act(async () => {
      render(<CanonicalMenu request={request} defer />);
    });
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
    expect(screen.getByTestId("rename-modal-input")).toHaveProperty("value", "Ethiopians");
    fireEvent.change(screen.getByTestId("rename-modal-input"), { target: { value: "Britons" } });
    const canonical = makeSession({ id: "s1", title: "Britons" });
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ ...canonical, outcome: { warnings: [] } }), {
        headers: {
          "Content-Type": "application/json",
          "aoe-runtime-epoch": "native-test",
          "aoe-runtime-revision": "2",
        },
      }),
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    });
    const renameCall = fetchSpy.mock.calls.find(
      ([url, init]) => String(url) === "/api/sessions/s1" && init?.method === "PATCH",
    );
    expect(renameCall?.[1]).toMatchObject({ method: "PATCH", body: JSON.stringify({ title: "Britons" }) });
    expect(screen.getByTestId("canonical-title").textContent).toBe("Ethiopians");
    expect(screen.getByTestId("canonical-cursor").textContent).toBe("native-test:1");
    expect(screen.getByRole("button", { name: "Rename" })).toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "Cancel" })).toHaveProperty("disabled", true);
    fireEvent.keyDown(document, { key: "Escape" });
    fireEvent.click(screen.getByTestId("rename-modal-backdrop"));
    expect(screen.getByTestId("rename-modal")).toBeTruthy();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Install canonical receipt" }));
    });
    expect(screen.getByTestId("canonical-title").textContent).toBe("Britons");
    expect(screen.getByTestId("canonical-cursor").textContent).toBe("native-test:2");
    expect(screen.queryByTestId("rename-modal")).toBeNull();
  });

  it("retains an unconfirmed rename when a new runtime rejects the old epoch receipt", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          sessions: workspaces[0]!.sessions,
          workspace_ordering: [],
        }),
        { headers: { "aoe-runtime-epoch": "native-test", "aoe-runtime-revision": "1" } },
      ),
    );
    const request: SessionMenuRequest = {
      key: 1,
      sessionId: "s1",
      x: 40,
      y: 50,
      items: [],
      source: "Board",
      onClose: vi.fn(),
    };
    await act(async () => {
      render(<CanonicalMenu request={request} />);
    });
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
    fireEvent.change(screen.getByTestId("rename-modal-input"), { target: { value: "Britons" } });
    const acknowledgement = Promise.withResolvers<Response>();
    fetchSpy.mockImplementationOnce(() => acknowledgement.promise);
    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    const current = makeSession({ id: "s1", title: "New runtime title" });
    fetchSpy.mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          sessions: [current],
          workspace_ordering: [],
        }),
        { headers: { "aoe-runtime-epoch": "new-runtime", "aoe-runtime-revision": "1" } },
      ),
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(screen.getByTestId("canonical-title").textContent).toBe("New runtime title");
    await act(async () => {
      acknowledgement.resolve(
        new Response(
          JSON.stringify({
            ...makeSession({ id: "s1", title: "Britons" }),
            outcome: { warnings: [] },
          }),
          { headers: { "aoe-runtime-epoch": "native-test", "aoe-runtime-revision": "2" } },
        ),
      );
    });
    expect(screen.getByTestId("canonical-title").textContent).toBe("New runtime title");
    expect(screen.getByTestId("canonical-cursor").textContent).toBe("new-runtime:1");
    expect(screen.getByTestId("rename-modal-input")).toHaveProperty("value", "Britons");
    expect(screen.getByRole("alert").textContent).toContain("outcome is unconfirmed");
    expect(screen.getByTestId("rename-modal")).toBeTruthy();
  });

  it.each(["outcome", "headers"])("refuses an untyped rename ACK missing %s", async (missing) => {
    open();
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
    fireEvent.change(screen.getByTestId("rename-modal-input"), { target: { value: "Britons" } });
    fetchSpy.mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          ...makeSession({ id: "s1", title: "Britons" }),
          ...(missing === "outcome" ? {} : { outcome: { warnings: [] } }),
        }),
        { headers: missing === "headers" ? {} : { "aoe-runtime-epoch": "native-test", "aoe-runtime-revision": "2" } },
      ),
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    });
    expect(handlers.onSessionMutation).not.toHaveBeenCalled();
    expect(screen.getByTestId("rename-modal")).toBeTruthy();
    expect(screen.getByTestId("rename-modal-input")).toHaveProperty("value", "Britons");
  });

  it("offers no write action on a read-only dashboard, rename included", () => {
    open({}, true);
    expect(screen.getByTestId("sidebar-context-menu")).toBeTruthy();
    for (const id of ["rename", "stop", "delete", "snooze"]) {
      expect(screen.queryByTestId(`sidebar-context-menu-${id}`)).toBeNull();
    }
  });

  it("snoozes the workspace with a picked preset", async () => {
    open();
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-snooze"));
    vi.useRealTimers();
    fireEvent.click(screen.getByRole("button", { name: "1 hour" }));
    expect(screen.queryByTestId("snooze-modal")).toBeNull();
    await waitFor(() => expect(String(fetchSpy.mock.calls[0]?.[0])).toContain("/api/sessions/s1/snooze"));
  });

  it.each([
    { item: "sidebar-context-menu-stop", called: () => handlers.onStop, args: ["s1"] },
    { item: "sidebar-context-menu-delete", called: () => handlers.onDelete, args: [["s1"]] },
  ])("$item calls back with the session", ({ item, called, args }) => {
    const onClose = open();
    arm();
    fireEvent.click(screen.getByTestId(item));
    expect(called()).toHaveBeenCalledExactlyOnceWith(...args);
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
  });

  it.each([
    ["sidebar-context-menu-archive", "/api/sessions/s1/archive"],
    ["sidebar-context-menu-pin", "/api/sessions/s1/pin"],
  ])("%s goes through the optimistic triage", async (item, path) => {
    open();
    arm();
    vi.useRealTimers();
    fireEvent.click(screen.getByTestId(item));
    await waitFor(() =>
      expect(fetchSpy.mock.calls.map(([url]) => String(url))).toContainEqual(expect.stringContaining(path)),
    );
  });

  it("acts only on the requested session of a workspace that groups several", async () => {
    const shared = [
      makeWorkspace("w1", [
        makeSession({ id: "s1", title: "First", status: "Running" }),
        makeSession({ id: "s2", title: "Second", status: "Running" }),
      ]),
    ];
    const show = () => {
      cleanup();
      const request = { key: 1, sessionId: "s2", x: 0, y: 0, items: [], source: "Board", onClose: vi.fn() };
      render(
        <PluginSessionMenu
          request={request}
          workspaces={shared}
          readOnly={false}
          handlers={handlers}
          {...observation}
        />,
      );
      arm();
    };

    show();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-stop"));
    expect(handlers.onStop).toHaveBeenCalledExactlyOnceWith("s2");
    show();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-delete"));
    expect(handlers.onDelete).toHaveBeenCalledExactlyOnceWith(["s2"]);
    show();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
    expect((screen.getByTestId("rename-modal-input") as HTMLInputElement).value).toBe("Second");
    fireEvent.change(screen.getByTestId("rename-modal-input"), { target: { value: "Renamed" } });
    show();
    vi.useRealTimers();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-archive"));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(fetchSpy.mock.calls.map(([url]) => String(url))).toEqual([
      expect.stringContaining("/api/sessions/s2/archive"),
    ]);
  });

  it("does not carry one session's optimistic triage to another in the same workspace", () => {
    const shared = [
      makeWorkspace("w1", [
        makeSession({ id: "s1", title: "First", status: "Running" }),
        makeSession({ id: "s2", title: "Second", status: "Running" }),
      ]),
    ];
    const request = (key: number, sessionId: string) => ({
      key,
      sessionId,
      x: 0,
      y: 0,
      items: [],
      source: "Board",
      onClose: vi.fn(),
    });
    const { rerender } = render(
      <PluginSessionMenu
        request={request(1, "s1")}
        workspaces={shared}
        readOnly={false}
        handlers={handlers}
        {...observation}
      />,
    );
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-pin"));
    // The server has not reported s1 pinned yet, so the pin is still only optimistic.
    rerender(
      <PluginSessionMenu
        request={request(2, "s2")}
        workspaces={shared}
        readOnly={false}
        handlers={handlers}
        {...observation}
      />,
    );
    arm();
    expect(screen.getByTestId("sidebar-context-menu-pin").getAttribute("aria-pressed")).toBe("false");
  });

  it("opens the group picker from More", () => {
    open();
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-more"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-edit-group"));
    expect(screen.getByTestId("session-group-modal-input")).toBeTruthy();
  });
});
