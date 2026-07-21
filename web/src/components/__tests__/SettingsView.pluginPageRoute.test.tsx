// @vitest-environment jsdom
//
// Route-gating for plugin settings pages (#2985): a `plugin-page:` URL only
// renders the page when it resolves to a declared, enabled contribution. A
// stale/typo'd/disabled route must fall back to the built-in default once the
// plugin list has loaded, never a permanent "Waiting…" page.

import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import { pluginPageTabId } from "../SettingsView";

const PROFILES = [{ name: "main", is_default: true }];

const PLUGINS = {
  plugins: [
    {
      id: "acme.mcp",
      name: "MCP",
      enabled: true,
      ui_contributions: [{ slot: "settings-page", id: "servers" }],
    },
  ],
  load_errors: [],
};

const { fetchPluginsMock } = vi.hoisted(() => ({ fetchPluginsMock: vi.fn() }));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchPlugins: fetchPluginsMock,
  fetchSettings: vi.fn(() => Promise.resolve({ acp: {}, sandbox: {}, worktree: {} })),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
}));

afterEach(() => {
  fetchPluginsMock.mockReset();
  window.localStorage.clear();
});

describe("SettingsView plugin-page route gating", () => {
  it("renders the page (waiting state) for a route matching a declared contribution", async () => {
    fetchPluginsMock.mockResolvedValue(PLUGINS);
    render(
      <SettingsView
        onClose={() => {}}
        tab={pluginPageTabId("acme.mcp", "servers")}
        onSelectTab={vi.fn()}
        onServerAboutRefresh={() => {}}
      />,
    );
    // No UI-state pushed yet, so the page shows its explicit waiting state.
    expect(await screen.findByTestId("plugin-settings-page-waiting")).toBeTruthy();
  });

  it("falls back (no permanent waiting page) for a route absent from the loaded nav", async () => {
    fetchPluginsMock.mockResolvedValue(PLUGINS);
    render(
      <SettingsView
        onClose={() => {}}
        tab={pluginPageTabId("ghost.kit", "nope")}
        onSelectTab={vi.fn()}
        onServerAboutRefresh={() => {}}
      />,
    );
    // Once the plugin list resolves, an unmatched route must not show the
    // plugin-page waiting state; it falls through to the built-in default tab.
    await waitFor(() => expect(fetchPluginsMock).toHaveBeenCalled());
    await waitFor(() => expect(screen.queryByTestId("plugin-settings-page-waiting")).toBeNull());
    expect(screen.queryByTestId("plugin-settings-page")).toBeNull();
  });
});
