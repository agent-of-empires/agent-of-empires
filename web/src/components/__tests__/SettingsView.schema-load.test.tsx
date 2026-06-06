// @vitest-environment jsdom
//
// Regression for the schema-load failure path (#1692 / CodeRabbit): if
// getSettingsSchema() fails, the schema-driven Worktree tab must show an error
// and a Retry that recovers, instead of rendering a permanently blank tab.

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [{ name: "main", is_default: true }];

const WORKTREE_SCHEMA = [
  {
    section: "worktree",
    field: "path_template",
    category: "Worktree",
    label: "Path Template",
    description: "",
    widget: { kind: "text" },
    web_write: { policy: "requires_elevation", reason: "host filesystem" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  },
];

const DISCORD_SCHEMA = [
  {
    section: "discord",
    field: "webhook_url",
    category: "Discord Beta",
    label: "Webhook URL",
    description: "Discord webhook URL for status notifications.",
    widget: { kind: "optional_text" },
    web_write: { policy: "writable" },
    profile_overridable: false,
    validation: { rule: "none" },
    advanced: false,
  },
];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() => Promise.resolve({ worktree: {} })),
  // First load fails (returns null), retry succeeds. Closure reads
  // WORKTREE_SCHEMA lazily (the mock factory is hoisted above the const).
  getSettingsSchema: (() => {
    let calls = 0;
    return vi.fn(() => Promise.resolve(calls++ === 0 ? null : WORKTREE_SCHEMA));
  })(),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  updateSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

function renderView(tab: string) {
  return render(
    <SettingsView
      onClose={() => {}}
      tab={tab}
      onSelectTab={vi.fn()}
      onServerAboutRefresh={() => {}}
    />,
  );
}

function findTextInputByLabel(
  container: HTMLElement,
  label: string,
): HTMLInputElement {
  const labels = container.querySelectorAll("label");
  for (const node of labels) {
    if (node.textContent === label) {
      const input = node.parentElement?.querySelector("input[type='text']");
      if (input instanceof HTMLInputElement) return input;
    }
  }
  throw new Error(`text input with label ${label} not found`);
}

describe("SettingsView schema load", () => {
  it("shows an error + retry when the schema fails, then recovers", async () => {
    renderView("worktree");

    // First fetch returned null -> error surfaced, not a blank tab.
    await waitFor(() =>
      expect(screen.getByText("Failed to load settings schema.")).toBeTruthy(),
    );
    const retry = screen.getByRole("button", { name: "Retry" });

    // Retry refetches; the second call returns the schema and fields render.
    fireEvent.click(retry);
    await waitFor(() => expect(screen.getByText("Path Template")).toBeTruthy());
    expect(screen.queryByText("Failed to load settings schema.")).toBeNull();
    expect(vi.mocked(api.getSettingsSchema)).toHaveBeenCalledTimes(2);
  });

  it("saves non-profile-overridable schema fields through the global settings path", async () => {
    vi.mocked(api.getSettingsSchema).mockResolvedValueOnce(
      DISCORD_SCHEMA as never,
    );
    vi.mocked(api.fetchSettings).mockResolvedValueOnce({
      discord: { webhook_url: null },
    } as never);

    const { container } = renderView("discord");
    await screen.findByText("Webhook URL");

    const input = findTextInputByLabel(container, "Webhook URL");
    fireEvent.focus(input);
    fireEvent.change(input, {
      target: { value: "https://discord.com/api/webhooks/123/abc" },
    });
    fireEvent.blur(input);

    await waitFor(() =>
      expect(vi.mocked(api.updateSettings)).toHaveBeenCalledWith({
        discord: { webhook_url: "https://discord.com/api/webhooks/123/abc" },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).not.toHaveBeenCalledWith(
      "main",
      expect.objectContaining({ discord: expect.anything() }),
    );
  });

  it("keeps a mixed tab's non-schema rows visible when the schema fails", async () => {
    // The session tab mixes a non-schema row (the default-profile selector)
    // with a SchemaSection. A schema-load failure must only blank the schema
    // slot, not the whole tab (CodeRabbit #1987).
    vi.mocked(api.getSettingsSchema).mockResolvedValue(null);
    renderView("session");

    await waitFor(() =>
      expect(screen.getByText("Failed to load settings schema.")).toBeTruthy(),
    );
    // The non-schema selector is still there alongside the schema-slot error.
    expect(screen.getByText("Default profile")).toBeTruthy();
  });
});
