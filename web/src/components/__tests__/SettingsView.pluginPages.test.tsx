// @vitest-environment jsdom
//
// Unit coverage for the plugin settings-page nav derivation and routing helpers
// (#2985): a plugin declaring the `settings-page` UI slot gets one Settings nav
// entry per declared contribution, addressed by a parametric `plugin-page:<...>`
// tab id that round-trips as a single URL segment.

import { describe, expect, it } from "vitest";
import { buildSidebar, parsePluginPageTab, pluginSettingsPages } from "../SettingsView";

type Plugin = Parameters<typeof pluginSettingsPages>[0][number];

function plugin(over: Partial<Plugin>): Plugin {
  return {
    id: "acme.mcp",
    name: "MCP",
    enabled: true,
    ui_contributions: [{ slot: "settings-page", id: "main" }],
    ...over,
  };
}

describe("pluginSettingsPages", () => {
  it("derives one nav entry per declared settings-page contribution", () => {
    const pages = pluginSettingsPages([plugin({})]);
    expect(pages).toHaveLength(1);
    expect(pages[0]).toMatchObject({ label: "MCP", pluginId: "acme.mcp", contribId: "main" });
    // Parses back to the same identifiers.
    expect(parsePluginPageTab(pages[0].tabId)).toEqual({ pluginId: "acme.mcp", contribId: "main" });
  });

  it("ignores disabled plugins and non-settings-page slots", () => {
    const pages = pluginSettingsPages([
      plugin({ id: "acme.off", enabled: false }),
      plugin({ id: "acme.pane", ui_contributions: [{ slot: "pane", id: "x" }] }),
    ]);
    expect(pages).toHaveLength(0);
  });

  it("disambiguates the label when a plugin declares multiple pages", () => {
    const pages = pluginSettingsPages([
      plugin({
        ui_contributions: [
          { slot: "settings-page", id: "servers" },
          { slot: "settings-page", id: "tools" },
        ],
      }),
    ]);
    expect(pages.map((p) => p.label)).toEqual(["MCP: servers", "MCP: tools"]);
  });

  it("sorts entries deterministically by label", () => {
    const pages = pluginSettingsPages([plugin({ id: "z.kit", name: "Zeta" }), plugin({ id: "a.kit", name: "Alpha" })]);
    expect(pages.map((p) => p.label)).toEqual(["Alpha", "Zeta"]);
  });
});

describe("parsePluginPageTab", () => {
  it("returns null for built-in tabs and malformed ids", () => {
    expect(parsePluginPageTab("mcp")).toBeNull();
    expect(parsePluginPageTab(null)).toBeNull();
    expect(parsePluginPageTab("plugin-page:onlyone")).toBeNull();
  });

  it("round-trips ids containing dots and reserved chars", () => {
    // A plugin id has dots; encodeURIComponent keeps the delimiter unambiguous
    // even if a contribution id itself contains a colon.
    const [page] = pluginSettingsPages([
      plugin({ id: "acme.mcp.v2", ui_contributions: [{ slot: "settings-page", id: "a:b" }] }),
    ]);
    expect(parsePluginPageTab(page.tabId)).toEqual({ pluginId: "acme.mcp.v2", contribId: "a:b" });
  });
});

describe("buildSidebar", () => {
  it("appends a Plugin pages divider and one tab per page", () => {
    const pages = pluginSettingsPages([plugin({})]);
    const sidebar = buildSidebar(pages);
    const divider = sidebar.find((s) => s.kind === "divider" && s.label === "Plugin pages");
    expect(divider).toBeTruthy();
    expect(sidebar.some((s) => s.kind === "tab" && s.id === pages[0].tabId)).toBe(true);
  });

  it("adds no divider when there are no plugin pages", () => {
    const sidebar = buildSidebar([]);
    expect(sidebar.some((s) => s.kind === "divider" && s.label === "Plugin pages")).toBe(false);
  });
});
