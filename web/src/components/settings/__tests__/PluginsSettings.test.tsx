// @vitest-environment jsdom
//
// Contract test for the minimal PluginsSettings panel: it lists plugins
// (name, version, description, enabled state), the enable toggle POSTs the
// right setPluginEnabled payload, the server-returned refreshed list is
// adopted on success, a toggle error message is surfaced, and load_errors are
// shown rather than swallowed.

import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";

import type { DiscoverResult, PluginListResponse, PluginToggleResult, PluginUpdateStatus } from "../../../lib/api";

const fetchPlugins = vi.fn<[], Promise<PluginListResponse | null>>();
const setPluginEnabled = vi.fn<[string, boolean], Promise<PluginToggleResult>>();
const fetchPluginUpdates = vi.fn<[], Promise<{ updates: PluginUpdateStatus[] } | null>>();
const discoverPlugins = vi.fn<[string], Promise<DiscoverResult>>();
const reportInfo = vi.fn<[string], void>();

vi.mock("../../../lib/api", () => ({
  fetchPlugins: () => fetchPlugins(),
  setPluginEnabled: (id: string, enabled: boolean) => setPluginEnabled(id, enabled),
  fetchPluginUpdates: () => fetchPluginUpdates(),
  discoverPlugins: (q: string) => discoverPlugins(q),
}));

vi.mock("../../../lib/toastBus", () => ({
  reportInfo: (message: string) => reportInfo(message),
}));

// Imported after the mock is registered.
import { PluginsSettings } from "../PluginsSettings";

function listResponse(overrides: Partial<PluginListResponse> = {}): PluginListResponse {
  return {
    plugins: [
      {
        id: "aoe.status",
        name: "Agent Status Detection",
        version: "1.1.0",
        description: "Detects agent session status.",
        enabled: true,
        builtin: true,
        validation: "builtin",
        source: null,
        capabilities: [],
        ui_contributions: [],
        granted: true,
        needs_reapproval: false,
      },
      {
        id: "example.plugin",
        name: "Example",
        version: "0.1.0",
        description: "A community plugin.",
        enabled: false,
        builtin: false,
        validation: "community",
        source: "gh:example/plugin",
        capabilities: ["net"],
        ui_contributions: [
          { slot: "status-bar", id: "s" },
          { slot: "row-badge", id: "b" },
        ],
        granted: true,
        needs_reapproval: false,
      },
    ],
    load_errors: [],
    ...overrides,
  };
}

beforeEach(() => {
  fetchPlugins.mockReset();
  setPluginEnabled.mockReset();
  fetchPluginUpdates.mockReset();
  discoverPlugins.mockReset();
  reportInfo.mockReset();
  fetchPlugins.mockResolvedValue(listResponse());
  fetchPluginUpdates.mockResolvedValue({ updates: [] });
  discoverPlugins.mockResolvedValue({ kind: "ok", results: [] });
});

describe("PluginsSettings", () => {
  it("renders each plugin's name, version, and description", async () => {
    const { findByText } = render(<PluginsSettings />);
    await findByText("Agent Status Detection");
    await findByText("v1.1.0");
    await findByText("A community plugin.");
  });

  it("discloses the UI slots a plugin renders into, deduped", async () => {
    const { findByText } = render(<PluginsSettings />);
    // example.plugin declares status-bar + row-badge (#2366).
    await findByText("UI: status-bar, row-badge");
  });

  it("shows validation badges and a needs-approval state for an ungranted community plugin", async () => {
    fetchPlugins.mockResolvedValue(
      listResponse({
        plugins: [
          {
            id: "example.plugin",
            name: "Example",
            version: "0.2.0",
            description: "A community plugin.",
            enabled: true,
            builtin: false,
            validation: "community",
            source: "gh:example/plugin",
            capabilities: ["net", "fs.read"],
            granted: false,
            needs_reapproval: true,
          },
        ],
      }),
    );
    const { findByTestId, getByText } = render(<PluginsSettings />);
    const validation = await findByTestId("plugin-validation-example.plugin");
    expect(validation.textContent).toBe("community");
    await findByTestId("plugin-needs-approval-example.plugin");
    expect(getByText(/net, fs\.read/)).toBeTruthy();
    expect(getByText(/not granted/)).toBeTruthy();
  });

  it("shows the featured validation badge for a featured plugin", async () => {
    fetchPlugins.mockResolvedValue(
      listResponse({
        plugins: [
          {
            id: "agent-of-empires.example",
            name: "Official Example",
            version: "1.0.0",
            description: "A featured plugin.",
            enabled: true,
            builtin: false,
            validation: "featured",
            source: "gh:agent-of-empires/example",
            capabilities: [],
            granted: true,
            needs_reapproval: false,
          },
        ],
      }),
    );
    const { findByTestId } = render(<PluginsSettings />);
    const validation = await findByTestId("plugin-validation-agent-of-empires.example");
    expect(validation.textContent).toBe("featured");
  });

  it("disable toggle POSTs setPluginEnabled(id, false) and adopts the refreshed list", async () => {
    const disabled = listResponse({
      plugins: [{ ...listResponse().plugins[0]!, enabled: false }, listResponse().plugins[1]!],
    });
    setPluginEnabled.mockResolvedValue({ kind: "ok", data: disabled });

    const { findByLabelText } = render(<PluginsSettings />);
    const toggle = (await findByLabelText("Enable Agent Status Detection")) as HTMLInputElement;
    expect(toggle.checked).toBe(true);
    fireEvent.click(toggle);

    await waitFor(() => {
      expect(setPluginEnabled).toHaveBeenCalledWith("aoe.status", false);
    });
    await waitFor(() => {
      expect((toggle as HTMLInputElement).checked).toBe(false);
    });
  });

  it("warns about the startup-only serve gate when aoe.web is disabled", async () => {
    const web = {
      id: "aoe.web",
      name: "Web Dashboard",
      version: "1.0.0",
      description: "The web dashboard.",
      enabled: true,
      builtin: true,
      validation: "builtin",
      source: null,
      capabilities: [],
      granted: true,
      needs_reapproval: false,
    };
    fetchPlugins.mockResolvedValue(listResponse({ plugins: [web] }));
    setPluginEnabled.mockResolvedValue({
      kind: "ok",
      data: listResponse({ plugins: [{ ...web, enabled: false }] }),
    });

    const { findByLabelText } = render(<PluginsSettings />);
    fireEvent.click(await findByLabelText("Enable Web Dashboard"));

    await waitFor(() => {
      expect(reportInfo).toHaveBeenCalledWith("Web dashboard stays up until aoe serve is restarted.");
    });
  });

  it("does not warn when a non-web plugin is disabled", async () => {
    const disabled = listResponse({
      plugins: [{ ...listResponse().plugins[0]!, enabled: false }, listResponse().plugins[1]!],
    });
    setPluginEnabled.mockResolvedValue({ kind: "ok", data: disabled });
    const { findByLabelText } = render(<PluginsSettings />);
    fireEvent.click(await findByLabelText("Enable Agent Status Detection"));
    await waitFor(() => {
      expect(setPluginEnabled).toHaveBeenCalledWith("aoe.status", false);
    });
    expect(reportInfo).not.toHaveBeenCalled();
  });

  it("surfaces the error message when a toggle is rejected", async () => {
    setPluginEnabled.mockResolvedValue({ kind: "error", message: "Dashboard is read-only." });
    const { findByLabelText, findByText } = render(<PluginsSettings />);
    fireEvent.click(await findByLabelText("Enable Agent Status Detection"));
    await findByText("Dashboard is read-only.");
  });

  it("renders an explicit empty state when there are no plugins", async () => {
    fetchPlugins.mockResolvedValue(listResponse({ plugins: [] }));
    const { getByTestId, findByTestId } = render(<PluginsSettings />);
    await findByTestId("plugins-empty");
    expect(getByTestId("plugins-empty").textContent).toContain("No plugins detected");
  });

  it("surfaces load_errors rather than swallowing them", async () => {
    fetchPlugins.mockResolvedValue(listResponse({ load_errors: ["plugins/bad: manifest is invalid"] }));
    const { findByText } = render(<PluginsSettings />);
    await findByText(/manifest is invalid/);
  });

  it("shows an error when the plugin list fails to load", async () => {
    fetchPlugins.mockResolvedValue(null);
    const { findByText } = render(<PluginsSettings />);
    await findByText("Failed to load plugins.");
  });

  it("Check for updates calls the endpoint and badges an outdated plugin", async () => {
    fetchPluginUpdates.mockResolvedValue({
      updates: [
        {
          id: "example.plugin",
          source: "gh:example/plugin",
          current: "abc1234",
          available: "def5678",
          needs_update: true,
          error: null,
        },
      ],
    });
    const { findByTestId, getByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    await waitFor(() => expect(fetchPluginUpdates).toHaveBeenCalled());
    await findByTestId("plugin-update-available-example.plugin");
    expect(getByTestId("plugin-example.plugin").textContent).toContain("abc1234 → def5678");
  });

  it("Check for updates surfaces a per-plugin check error", async () => {
    fetchPluginUpdates.mockResolvedValue({
      updates: [
        {
          id: "example.plugin",
          source: "gh:example/plugin",
          current: "",
          available: null,
          needs_update: false,
          error: "git not found",
        },
      ],
    });
    const { findByTestId, findByText } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    await findByText(/Update check failed: git not found/);
  });

  it("Search GitHub renders badged results with a copyable install command", async () => {
    discoverPlugins.mockResolvedValue({
      kind: "ok",
      results: [
        {
          slug: "gh:acme/widget",
          html_url: "https://github.com/acme/widget",
          description: "A widget plugin.",
          stars: 42,
          badge: "unvetted",
          install_command: "aoe plugin install gh:acme/widget",
        },
      ],
    });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-discover"));
    await waitFor(() => expect(discoverPlugins).toHaveBeenCalled());
    const result = await findByTestId("plugins-discover-result-gh:acme/widget");
    expect(result.textContent).toContain("aoe plugin install gh:acme/widget");
    expect(result.textContent).toContain("unvetted");
  });

  it("Search GitHub surfaces a discovery error (e.g. rate limit)", async () => {
    discoverPlugins.mockResolvedValue({ kind: "error", message: "Rate limited by GitHub." });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-discover"));
    const err = await findByTestId("plugins-discover-error");
    expect(err.textContent).toContain("Rate limited by GitHub.");
  });
});
