// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import { resetSettingsStore } from "../../lib/settingsStore";

const PROFILES = [{ name: "main", is_default: true }];

const MCP_SCHEMA = [
  {
    section: "web",
    field: "mcp_enabled",
    category: "Web",
    label: "MCP server",
    description: "Serve a Model Context Protocol server at `/mcp` on the daemon.",
    widget: { kind: "toggle" },
    web_write: { policy: "allow" },
    profile_overridable: false,
    validation: { rule: "none" },
    advanced: false,
  },
];

const updateSettings = vi.fn(() => Promise.resolve("committed" as const));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(() => Promise.resolve({ web: { mcp_enabled: false } })),
  getSettingsSchema: vi.fn(() => Promise.resolve(MCP_SCHEMA)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
  updateSettings: (updates: Record<string, unknown>, profile?: string) => updateSettings(updates, profile),
}));

vi.mock("../../hooks/usePushSubscription", () => ({
  usePushSubscription: () => ({
    state: { kind: "off" },
    health: "unknown",
    enable: vi.fn(),
    disable: vi.fn(),
    sendTest: vi.fn(),
    refresh: vi.fn(),
  }),
}));

beforeEach(resetSettingsStore);

afterEach(() => {
  cleanup();
  updateSettings.mockClear();
});

describe("SettingsView MCP server toggle", () => {
  it("renders as a global-only switch and saves { web: { mcp_enabled } }", async () => {
    render(
      <SettingsView onClose={() => {}} tab="notifications" onSelectTab={vi.fn()} onServerAboutRefresh={() => {}} />,
    );

    await screen.findByText("MCP server");
    expect(screen.getByText(/Applies to all profiles \(not profile-overridable\)\./)).toBeTruthy();

    const toggle = screen.getByRole("switch");
    expect(toggle.getAttribute("aria-checked")).toBe("false");

    fireEvent.click(toggle);

    await waitFor(() => expect(updateSettings).toHaveBeenCalledWith({ web: { mcp_enabled: true } }, "main"));
    await waitFor(() => expect(toggle.getAttribute("aria-checked")).toBe("true"));
  });
});
