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
    await waitFor(() =>
      expect(screen.getByText("Path Template")).toBeTruthy(),
    );
    expect(screen.queryByText("Failed to load settings schema.")).toBeNull();
    expect(vi.mocked(api.getSettingsSchema)).toHaveBeenCalledTimes(2);
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
