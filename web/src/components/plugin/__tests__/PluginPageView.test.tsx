// @vitest-environment jsdom

import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginPage } from "../../../lib/api";
import type { ResolvedTheme } from "../../../lib/theme";
import { THEME_CHANGED_EVENT } from "../../../lib/theme";
import { PluginPageView } from "../PluginPageView";

const h = vi.hoisted(() => ({
  pages: null as PluginPage[] | null,
  mint: vi.fn<(url: string) => Promise<boolean>>(),
  sessions: vi.fn(),
  action: vi.fn(),
  archive: vi.fn(),
  trash: vi.fn(),
}));
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
});

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
      />,
    );
    await request({ aoeBridge: 1, id: 2, method: "action", params: { method: "move" } });
    expect(port.postMessage).toHaveBeenLastCalledWith(
      expect.objectContaining({ id: 2, error: expect.objectContaining({ code: "read_only" }) }),
    );
    expect(h.action).not.toHaveBeenCalled();
  });
});
