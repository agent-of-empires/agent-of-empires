// @vitest-environment jsdom

import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginPage } from "../../../lib/api";
import type { ResolvedTheme } from "../../../lib/theme";
import { THEME_CHANGED_EVENT } from "../../../lib/theme";
import { PluginPageView } from "../PluginPageView";

const h = vi.hoisted(() => ({
  pages: null as PluginPage[] | null,
  mint: vi.fn<(page: PluginPage) => Promise<boolean>>(),
  sessions: vi.fn(),
}));
vi.mock("../../../lib/pluginUiContext", () => ({ usePluginPages: () => h.pages }));
vi.mock("../../../lib/api", () => ({
  mintPluginPageTicket: (page: PluginPage) => h.mint(page),
  fetchSessions: () => h.sessions(),
  fetchPluginUiState: vi.fn(),
  invokePluginAction: vi.fn(),
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
  return { ...utils, frame, send };
}

beforeEach(() => {
  h.pages = [board];
  h.mint.mockReset();
  h.sessions.mockReset();
});

describe("PluginPageView", () => {
  it("frames the page, sandboxed to scripts only, once its ticket is minted", async () => {
    let resolve!: (ok: boolean) => void;
    h.mint.mockReturnValue(new Promise((r) => (resolve = r)));
    view();
    expect(h.mint).toHaveBeenCalledWith(board);
    expect(screen.queryByTestId("plugin-page-frame")).toBeNull();
    resolve(true);
    const frame = await screen.findByTestId("plugin-page-frame");
    expect(frame.getAttribute("sandbox")).toBe("allow-scripts");
    expect(frame.getAttribute("src")).toBe(board.url);
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
    h.sessions.mockResolvedValue({ sessions: [{ id: "s" }], workspace_ordering: [] });
    const { send } = await openFrame();
    const foreign = fakePort();
    await send({ aoeBridge: 1, id: 1, method: "ready" }, [foreign.port], window);
    await send({ aoeBridge: 1, id: 2, method: "sessions.list" }, [foreign.port]);
    expect(foreign.port.onmessage).toBeNull();

    const { port, request } = fakePort();
    await send({ aoeBridge: 1, id: "r", method: "ready" }, [port]);
    expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: "r", result: null });
    const late = fakePort();
    await send({ aoeBridge: 1, id: "r2", method: "ready" }, [late.port]);
    expect(late.port.onmessage).toBeNull();

    await request({ aoeBridge: 1, id: 3, method: "sessions.list" });
    await waitFor(() => expect(port.postMessage).toHaveBeenCalledWith({ aoeBridge: 1, id: 3, result: [{ id: "s" }] }));
    expect(h.sessions).toHaveBeenCalledTimes(1);
  });

  it("pushes context and theme after ready, and every later theme change", async () => {
    const { send } = await openFrame();
    const { port } = fakePort();
    await send({ aoeBridge: 1, id: "r", method: "ready" }, [port]);
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
    const { frame, send } = await openFrame();
    const { port } = fakePort();
    fireEvent.load(frame);
    await send({ aoeBridge: 1, id: "r", method: "ready" }, [port]);
    expect(screen.getByTestId("plugin-page-frame")).toBe(frame);
    fireEvent.load(frame);
    expect(port.close).toHaveBeenCalled();
    expect(screen.queryByTestId("plugin-page-frame")).toBeNull();
    fireEvent.click(screen.getByText("Reopen"));
    expect(h.mint).toHaveBeenCalledTimes(2);
    expect(await screen.findByTestId("plugin-page-frame")).not.toBe(frame);
  });

  it("starts a remounted frame's load count afresh", async () => {
    const { frame, rerender } = await openFrame();
    fireEvent.load(frame);
    h.pages = [{ ...board }];
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
    const next = await waitFor(() => {
      const f = screen.getByTestId("plugin-page-frame");
      expect(f).not.toBe(frame);
      return f;
    });
    fireEvent.load(next);
    expect(screen.getByTestId("plugin-page-frame")).toBe(next);
  });
});
