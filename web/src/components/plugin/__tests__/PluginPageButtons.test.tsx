// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, expect, it, vi } from "vitest";
import type { PluginPage } from "../../../lib/api";
import { PluginPageButtons } from "../PluginPageButtons";

const h = vi.hoisted(() => ({ pages: null as PluginPage[] | null, navigate: vi.fn() }));
vi.mock("../../../lib/pluginUiContext", () => ({ usePluginPages: () => h.pages }));
vi.mock("../../../lib/sessionRoute", () => ({ requestNavigate: (path: string) => h.navigate(path) }));

const page = (id: string, title: string): PluginPage => ({
  plugin_id: "acme.kit",
  id,
  title,
  icon: "kanban",
  url: `/api/plugins/acme.kit/pages/${id}`,
  capabilities: [],
});

afterEach(cleanup);

it("renders one button per page, marks the open one, and navigates on click", () => {
  h.pages = [page("board", "Board"), page("stats", "Stats")];
  render(
    <MemoryRouter initialEntries={["/plugins/acme.kit/board"]}>
      <PluginPageButtons />
    </MemoryRouter>,
  );
  expect(screen.getByRole("button", { name: "Board" }).getAttribute("aria-current")).toBe("page");
  expect(screen.getByRole("button", { name: "Stats" }).getAttribute("aria-current")).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Stats" }));
  expect(h.navigate).toHaveBeenCalledExactlyOnceWith("/plugins/acme.kit/stats");
});
