// @vitest-environment jsdom

import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginPage } from "../../../lib/api";
import { PluginPageView } from "../PluginPageView";

const h = vi.hoisted(() => ({
  pages: null as PluginPage[] | null,
  mint: vi.fn<(page: PluginPage) => Promise<boolean>>(),
}));
vi.mock("../../../lib/pluginUiContext", () => ({ usePluginPages: () => h.pages }));
vi.mock("../../../lib/api", () => ({ mintPluginPageTicket: (page: PluginPage) => h.mint(page) }));

const board: PluginPage = {
  plugin_id: "acme.kit",
  id: "board",
  title: "Board",
  icon: null,
  url: "/api/plugins/acme.kit/pages/board",
  capabilities: ["session.read"],
};

const view = () => render(<PluginPageView pluginId="acme.kit" pageId="board" onBack={() => {}} />);

beforeEach(() => {
  h.pages = [board];
  h.mint.mockReset();
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
});
