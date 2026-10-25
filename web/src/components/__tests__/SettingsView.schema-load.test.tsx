// @vitest-environment jsdom

import { describe, expect, it, vi, beforeEach } from "vitest";
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
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(() => Promise.resolve({ worktree: {} })),
  // First load fails (returns null), retry succeeds. Closure reads
  // WORKTREE_SCHEMA lazily (the mock factory is hoisted above the const).
  getSettingsSchema: (() => {
    let calls = 0;
    return vi.fn(() => Promise.resolve(calls++ === 0 ? null : WORKTREE_SCHEMA));
  })(),
  updateSettings: vi.fn(() => Promise.resolve("committed" as const)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

function renderView(tab: string, field?: string) {
  return render(
    <SettingsView onClose={() => {}} tab={tab} onSelectTab={vi.fn()} onServerAboutRefresh={() => {}} field={field} />,
  );
}

import { resetSettingsStore } from "../../lib/settingsStore";

beforeEach(resetSettingsStore);

describe("SettingsView schema load", () => {
  it("shows an error + retry when the schema fails, then recovers", async () => {
    Element.prototype.scrollIntoView = vi.fn();
    renderView("worktree", "worktree.path_template");

    // First fetch returned null -> error surfaced, not a blank tab.
    await waitFor(() => expect(screen.getByText("Failed to load settings schema.")).toBeTruthy());
    const retry = screen.getByRole("button", { name: "Retry" });

    // Retry refetches; the second call returns the schema and fields render.
    fireEvent.click(retry);
    await waitFor(() => expect(screen.getByText("Path Template")).toBeTruthy());
    expect(screen.queryByText("Failed to load settings schema.")).toBeNull();
    expect(vi.mocked(api.getSettingsSchema)).toHaveBeenCalledTimes(2);
    // A ?field= deep link waits for the successful load, so Retry completes it.
    expect(document.querySelector(".animate-settings-highlight")?.getAttribute("data-settings-field")).toBe(
      "worktree.path_template",
    );
  });

  it("keeps a mixed tab's non-schema rows visible when the schema fails", async () => {
    // The session tab mixes a non-schema row (the default-profile selector) with a SchemaSection.
    vi.mocked(api.getSettingsSchema).mockResolvedValue(null);
    renderView("session");

    await waitFor(() => expect(screen.getByText("Failed to load settings schema.")).toBeTruthy());
    // The non-schema selector is still there alongside the schema-slot error.
    expect(screen.getByText("Default profile")).toBeTruthy();
  });
});
