// @vitest-environment jsdom

import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginPage } from "../../../lib/api";
import type { ResolvedTheme } from "../../../lib/theme";
import { THEME_CHANGED_EVENT } from "../../../lib/theme";
import type { ReactNode } from "react";
import { PluginPageView } from "../PluginPageView";
import type { SessionMenuRequest } from "../PluginSessionMenu";

const h = vi.hoisted(() => ({
  pages: null as PluginPage[] | null,
  mint: vi.fn<(url: string) => Promise<boolean>>(),
  sessions: vi.fn(),
  action: vi.fn(),
  archive: vi.fn(),
  trash: vi.fn(),
  openSession: vi.fn(),
  renderSession: vi.fn<(id: string) => ReactNode>(),
  renderSessionMenu: vi.fn<(request: SessionMenuRequest) => ReactNode>(),
}));
vi.mock("../../../hooks/useIsWideViewport", () => ({ useIsWideViewport: () => true }));
vi.mock("../../../lib/sessionRoute", () => ({ requestOpenSession: (id: string) => h.openSession(id) }));
vi.mock("../../../lib/pluginUiContext", () => ({ usePluginPages: () => h.pages }));
vi.mock("../../../lib/api", () => ({
  mintPluginPageTicket: (url: string) => h.mint(url),
  fetchPluginSessions: (pluginId: string) => h.sessions(pluginId),
  fetchPluginUiState: vi.fn(),
  invokePluginAction: (...args: unknown[]) => h.action(...args),
  setSessionArchive: (...args: unknown[]) => h.archive(...args),
  trashSession: (...args: unknown[]) => h.trash(...args),
}));

const board: PluginPage = {
  plugin_id: "acme.kit",
  id: "board",
  title: "Board",
  icon: null,
  url: "/api/plugins/acme.kit/pages/board",
  capabilities: ["session.read"],
};

const view = () =>
  render(
    <PluginPageView
      pluginId="acme.kit"
      pageId="board"
      onBack={() => {}}
      readOnly={false}
      cityhall={false}
      theme={null}
      renderSession={h.renderSession}
      renderSessionMenu={h.renderSessionMenu}
    />,
  );

const darkTheme = {
  name: "night",
  source: "builtin",
  appearance: "dark",
  web: { cssVars: { "--color-surface-900": "#000" } },
  terminal: { cssVars: { "--term-bg": "#111" } },
  syntax: { shikiTheme: "x" },
} as const satisfies ResolvedTheme;

function fakePort() {
  const port = { postMessage: vi.fn(), close: vi.fn(), onmessage: null as ((e: { data: unknown }) => void) | null };
  const request = (data: unknown) => act(() => port.onmessage?.({ data }));
  return { port, request };
}

async function openFrame() {
  h.mint.mockResolvedValue(true);
  const utils = view();
  const frame = (await screen.findByTestId("plugin-page-frame")) as HTMLIFrameElement;
  const win = frame.contentWindow!;
  // jsdom's MessageEvent rejects foreign ports, so attach the transferred port after construction.
  const send = (data: unknown, ports: unknown[] = [], source: Window | null = win) =>
    act(() => {
      const event = new MessageEvent("message", { data, source, origin: "null" });
      Object.defineProperty(event, "ports", { value: ports });
      window.dispatchEvent(event);
    });
  const nonce = new URL(frame.src).hash.slice(1);
  return { ...utils, frame, send, nonce };
}

beforeEach(() => {
  h.pages = [board];
  h.mint.mockReset();
  h.sessions.mockReset();
  h.action.mockReset();
  h.archive.mockReset();
  h.trash.mockReset();
  h.openSession.mockReset();
  h.renderSession.mockReset().mockImplementation((id) => <p>session {id}</p>);
  h.renderSessionMenu.mockReset().mockReturnValue(null);
  localStorage.clear();
});

async function readyFrame() {
  const opened = await openFrame();
  const channel = fakePort();
  await opened.send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce: opened.nonce } }, [channel.port]);
  return { ...opened, ...channel };
}

const pushes = (port: ReturnType<typeof fakePort>["port"], event: string) =>
  port.postMessage.mock.calls
    .map(([m]) => m)
    .filter((m) => m.event === event)
    .map((m) => m.data);

describe("PluginPageView", () => {
  it("frames the page, sandboxed to scripts only, once its ticket is minted", async () => {
    let resolve!: (ok: boolean) => void;
    h.mint.mockReturnValue(new Promise((r) => (resolve = r)));
    view();
    expect(h.mint).toHaveBeenCalledWith(board.url);
    expect(screen.queryByTestId("plugin-page-frame")).toBeNull();
    resolve(true);
    const frame = await screen.findByTestId("plugin-page-frame");
    expect(frame.getAttribute("sandbox")).toBe("allow-scripts");
    expect(frame.getAttribute("src")).toMatch(new RegExp(`^${board.url}#[0-9a-f]{32}$`));
  });

  it.each([
    { name: "a page no active plugin declares", pages: [], mintOk: true, text: "Page not found" },
    { name: "a failed ticket mint", pages: [board], mintOk: false, text: "This page could not be opened" },
  ])("shows an empty state for $name", async ({ pages, mintOk, text }) => {
    h.pages = pages;
    h.mint.mockResolvedValue(mintOk);
    view();
    expect(await screen.findByText(text)).toBeTruthy();
    expect(screen.queryByTestId("plugin-page-frame")).toBeNull();
  });

  it("waits for the first poll instead of reporting a deep-linked page missing", () => {
    h.pages = null;
    view();
    expect(screen.queryByTestId("plugin-page-empty")).toBeNull();
    expect(h.mint).not.toHaveBeenCalled();
  });

  it("hands the channel only to its own frame's ready, then serves requests over it", async () => {
    h.sessions.mockResolvedValue({ sessions: [{ id: "s" }] });
    const { send, nonce } = await openFrame();
    const foreign = fakePort();
    await send({ aoeBridge: 1, id: 1, method: "ready", params: { nonce } }, [foreign.port], window);
    await send({ aoeBridge: 1, id: 2, method: "sessions.list" }, [foreign.port]);
    expect(foreign.port.onmessage).toBeNull();

    const { port, request } = fakePort();
    await send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce } }, [port]);
    expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: "r", result: null });
    const late = fakePort();
    await send({ aoeBridge: 1, id: "r2", method: "ready", params: { nonce } }, [late.port]);
    expect(late.port.onmessage).toBeNull();

    await request({ aoeBridge: 1, id: 3, method: "sessions.list" });
    await waitFor(() => expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: 3, result: [{ id: "s" }] }));
    expect(h.sessions.mock.calls).toEqual([["acme.kit"]]);
  });

  it("hands the channel only to a ready that echoes this opening's nonce", async () => {
    const { send, nonce } = await openFrame();
    for (const params of [undefined, { nonce: "" }, { nonce: "0".repeat(32) }]) {
      const guess = fakePort();
      await send({ aoeBridge: 1, id: "g", method: "ready", params }, [guess.port]);
      expect(guess.port.onmessage).toBeNull();
    }
    const { port } = fakePort();
    await send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce } }, [port]);
    expect(port.onmessage).not.toBeNull();
  });

  it("pushes context and theme after ready, and every later theme change", async () => {
    const { send, nonce } = await openFrame();
    const { port } = fakePort();
    await send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce } }, [port]);
    expect(port.postMessage).toHaveBeenCalledWith({
      aoeBridge: 1,
      event: "context",
      data: { readOnly: false, cityhall: false, pluginId: "acme.kit", pageId: "board", bridgeVersion: 1 },
    });
    act(() => window.dispatchEvent(new CustomEvent(THEME_CHANGED_EVENT, { detail: darkTheme })));
    expect(port.postMessage).toHaveBeenLastCalledWith({
      aoeBridge: 1,
      event: "theme",
      data: { appearance: "dark", cssVars: { "--color-surface-900": "#000", "--term-bg": "#111" } },
    });
  });

  it("tears the frame down and closes the channel when it loads a second document, then reopens fresh", async () => {
    const { frame, send, nonce } = await openFrame();
    const { port } = fakePort();
    fireEvent.load(frame);
    await send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce } }, [port]);
    expect(screen.getByTestId("plugin-page-frame")).toBe(frame);
    fireEvent.load(frame);
    expect(port.close).toHaveBeenCalled();
    expect(screen.queryByTestId("plugin-page-frame")).toBeNull();
    fireEvent.click(screen.getByText("Reopen"));
    expect(h.mint).toHaveBeenCalledTimes(2);
    const reopened = await screen.findByTestId("plugin-page-frame");
    expect(reopened).not.toBe(frame);
    // A fresh frame's first load is not a reload.
    fireEvent.load(reopened);
    expect(screen.getByTestId("plugin-page-frame")).toBe(reopened);
  });

  it("keeps the frame and its ticket when a poll replaces the page object", async () => {
    const { frame, rerender } = await openFrame();
    fireEvent.load(frame);
    h.pages = [{ ...board, title: "Renamed" }];
    rerender(
      <PluginPageView
        pluginId="acme.kit"
        pageId="board"
        onBack={() => {}}
        readOnly={false}
        cityhall={false}
        theme={null}
        renderSession={h.renderSession}
        renderSessionMenu={h.renderSessionMenu}
      />,
    );
    expect(screen.getByTestId("plugin-page-frame")).toBe(frame);
    expect(h.mint).toHaveBeenCalledTimes(1);
  });

  it("archives and trashes the target session, replying null or failed", async () => {
    h.pages = [{ ...board, capabilities: ["session.read", "session.write"] }];
    h.sessions.mockResolvedValue({ sessions: [{ id: "s" }] });
    h.archive.mockResolvedValue({ id: "s" });
    h.trash.mockResolvedValueOnce({ id: "s" }).mockResolvedValueOnce(null);
    const { send, nonce } = await openFrame();
    const { port, request } = fakePort();
    await send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce } }, [port]);
    await request({ aoeBridge: 1, id: 2, method: "session.archive", params: { id: "s" } });
    await request({ aoeBridge: 1, id: 3, method: "session.trash", params: { id: "s" } });
    await waitFor(() => expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: 3, result: null }));
    await request({ aoeBridge: 1, id: 4, method: "session.trash", params: { id: "s" } });
    await waitFor(() =>
      expect(port.postMessage).toHaveBeenCalledWith(
        expect.objectContaining({ id: 4, error: expect.objectContaining({ code: "failed" }) }),
      ),
    );
    expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: 2, result: null });
    expect(h.archive.mock.calls).toEqual([["s", true]]);
    expect(h.trash.mock.calls).toEqual([["s"], ["s"]]);
  });

  it.each(["other", "../plugins/victim/uninstall#", "..", "%2e%2e", "s/../x"])(
    "refuses to archive or trash %s, which sessions.list does not return",
    async (id) => {
      h.pages = [{ ...board, capabilities: ["session.read", "session.write"] }];
      h.sessions.mockResolvedValue({ sessions: [{ id: "s" }] });
      const { send, nonce } = await openFrame();
      const { port, request } = fakePort();
      await send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce } }, [port]);
      await request({ aoeBridge: 1, id: 2, method: "session.archive", params: { id } });
      await request({ aoeBridge: 1, id: 3, method: "session.trash", params: { id } });
      await waitFor(() => {
        for (const reqId of [2, 3]) {
          expect(port.postMessage).toHaveBeenCalledWith(
            expect.objectContaining({ id: reqId, error: expect.objectContaining({ code: "invalid_params" }) }),
          );
        }
      });
      expect(h.archive).not.toHaveBeenCalled();
      expect(h.trash).not.toHaveBeenCalled();
    },
  );

  it("gates an open channel's requests on the current mode", async () => {
    const { send, rerender, nonce } = await openFrame();
    const { port, request } = fakePort();
    await send({ aoeBridge: 1, id: "r", method: "ready", params: { nonce } }, [port]);
    rerender(
      <PluginPageView
        pluginId="acme.kit"
        pageId="board"
        onBack={() => {}}
        readOnly={true}
        cityhall={false}
        theme={null}
        renderSession={h.renderSession}
        renderSessionMenu={h.renderSessionMenu}
      />,
    );
    await request({ aoeBridge: 1, id: 2, method: "action", params: { method: "move" } });
    expect(port.postMessage).toHaveBeenLastCalledWith(
      expect.objectContaining({ id: 2, error: expect.objectContaining({ code: "read_only" }) }),
    );
    expect(h.action).not.toHaveBeenCalled();
  });

  it("navigates on openSession, or with split view on, shows the session below and pushes the selection", async () => {
    const { port, request } = await readyFrame();
    expect(pushes(port, "selection")).toEqual([{ sessionId: null }]);
    await request({ aoeBridge: 1, id: 2, method: "openSession", params: { id: "s" } });
    expect(h.openSession.mock.calls).toEqual([["s"]]);

    fireEvent.click(screen.getByRole("button", { name: "Split view" }));
    expect(localStorage.getItem("aoe-plugin-page-split:acme.kit/board")).toBe("true");
    expect(screen.getByText("No session selected")).toBeTruthy();
    await request({ aoeBridge: 1, id: 3, method: "openSession", params: { id: "s" } });
    expect(screen.getByText("session s")).toBeTruthy();
    expect(h.openSession).toHaveBeenCalledTimes(1);
    expect(pushes(port, "selection")).toEqual([{ sessionId: null }, { sessionId: "s" }]);

    fireEvent.click(screen.getByRole("button", { name: "Open full" }));
    expect(h.openSession.mock.calls).toEqual([["s"], ["s"]]);
    fireEvent.click(screen.getByRole("button", { name: "Close session" }));
    expect(pushes(port, "selection").at(-1)).toEqual({ sessionId: null });

    await request({ aoeBridge: 1, id: 4, method: "openSession", params: { id: "gone" } });
    h.renderSession.mockReturnValue(null);
    fireEvent.click(screen.getByRole("button", { name: "Split view" }));
    fireEvent.click(screen.getByRole("button", { name: "Split view" }));
    expect(screen.getByText("Session not found")).toBeTruthy();
  });

  it("saves the split height a divider drag settles on, clamped", async () => {
    await readyFrame();
    fireEvent.click(screen.getByRole("button", { name: "Split view" }));
    const pane = screen.getByTestId("plugin-page-split-session");
    const container = pane.parentElement!;
    container.getBoundingClientRect = () => ({ bottom: 1000, height: 1000 }) as DOMRect;
    fireEvent.mouseDown(screen.getByTestId("plugin-page-split-resize"));
    expect(screen.getByTestId("plugin-page-frame").className).toContain("pointer-events-none");
    fireEvent.mouseMove(document, { clientY: 700 });
    expect(pane.style.height).toBe("30%");
    fireEvent.mouseMove(document, { clientY: 50 });
    fireEvent.mouseUp(document);
    expect(pane.style.height).toBe("80%");
    expect(localStorage.getItem("aoe-plugin-page-split-height")).toBe("0.8");
    expect(screen.getByTestId("plugin-page-frame").className).not.toContain("pointer-events-none");
  });

  it("resizes the split from the keyboard within its bounds, saving each step", async () => {
    await readyFrame();
    fireEvent.click(screen.getByRole("button", { name: "Split view" }));
    const divider = screen.getByRole("separator", { name: "Resize split" });
    const pane = screen.getByTestId("plugin-page-split-session");
    expect(divider.tabIndex).toBe(0);
    expect(divider.getAttribute("aria-valuenow")).toBe("50");

    fireEvent.keyDown(divider, { key: "ArrowUp" });
    expect(pane.style.height).toBe("55%");
    expect(divider.getAttribute("aria-valuenow")).toBe("55");
    expect(localStorage.getItem("aoe-plugin-page-split-height")).toBe("0.55");
    fireEvent.keyDown(divider, { key: "ArrowDown" });
    fireEvent.keyDown(divider, { key: "ArrowDown" });
    expect(pane.style.height).toBe("45%");
    fireEvent.keyDown(divider, { key: "End" });
    fireEvent.keyDown(divider, { key: "ArrowUp" });
    expect(pane.style.height).toBe("80%");
    fireEvent.keyDown(divider, { key: "Home" });
    expect(pane.style.height).toBe("20%");
    expect(localStorage.getItem("aoe-plugin-page-split-height")).toBe("0.2");
  });

  it("opens the host session menu at the frame's offset and replies with the pick", async () => {
    h.sessions.mockResolvedValue({ sessions: [{ id: "s" }] });
    const { frame, port, request } = await readyFrame();
    frame.getBoundingClientRect = () => ({ left: 100, top: 48 }) as DOMRect;
    const items = [{ id: "move:done", label: "Move to Done" }];
    await request({ aoeBridge: 1, id: 2, method: "session.contextMenu", params: { id: "s", x: 10, y: 20, items } });
    await waitFor(() => expect(h.renderSessionMenu).toHaveBeenCalled());
    const first = h.renderSessionMenu.mock.lastCall![0];
    expect(first).toMatchObject({ sessionId: "s", x: 110, y: 68, items });

    // A second menu supersedes the first, which resolves with nothing picked.
    await request({ aoeBridge: 1, id: 3, method: "session.contextMenu", params: { id: "s", x: 0, y: 0 } });
    await waitFor(() => expect(h.renderSessionMenu.mock.lastCall![0].key).toBe(first.key + 1));
    await waitFor(() => expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: 2, result: null }));
    act(() => h.renderSessionMenu.mock.lastCall![0].onClose("move:done"));
    await waitFor(() => expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: 3, result: "move:done" }));

    await request({ aoeBridge: 1, id: 4, method: "session.contextMenu", params: { id: "other", x: 0, y: 0 } });
    await waitFor(() =>
      expect(port.postMessage).toHaveBeenCalledWith(
        expect.objectContaining({ id: 4, error: expect.objectContaining({ code: "invalid_params" }) }),
      ),
    );
  });

  it("drops the host session menu when the frame loads a second document", async () => {
    h.sessions.mockResolvedValue({ sessions: [{ id: "s" }] });
    h.renderSessionMenu.mockReturnValue(<div data-testid="host-menu" />);
    const { frame, request } = await readyFrame();
    fireEvent.load(frame);
    await request({ aoeBridge: 1, id: 2, method: "session.contextMenu", params: { id: "s", x: 0, y: 0 } });
    await screen.findByTestId("host-menu");
    fireEvent.load(frame);
    expect(screen.queryByTestId("host-menu")).toBeNull();
  });
});
