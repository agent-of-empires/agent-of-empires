// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../../SettingsView";
import * as api from "../../../lib/api";
import { resetSettingsStore } from "../../../lib/settingsStore";
import { descriptor } from "./fixtures";

// The descriptor `sandbox.shared_credential` declares: a global-only, elevated select.
const SHARED_CREDENTIAL = descriptor({
  section: "sandbox",
  field: "shared_credential",
  category: "Sandbox",
  label: "Shared agent credential",
  widget: {
    kind: "select",
    options: [
      { value: "host_seed", label: "Seed from host login" },
      { value: "separate_login", label: "Separate sandbox login" },
    ],
  },
  web_write: {
    policy: "requires_elevation",
    reason: "sandbox credential handling affects host login isolation",
  },
  profile_overridable: false,
});

vi.mock("../../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve([{ name: "main", is_default: true }])),
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(() => Promise.resolve({ sandbox: { shared_credential: "host_seed" } })),
  getSettingsSchema: vi.fn(() => Promise.resolve([SHARED_CREDENTIAL])),
  updateSettings: vi.fn(() => Promise.resolve("committed" as const)),
  updateTheme: vi.fn(() => Promise.resolve(true)),
  fetchThemes: vi.fn(() => Promise.resolve([])),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

beforeEach(() => {
  resetSettingsStore();
  vi.clearAllMocks();
});

describe("sandbox.shared_credential setting", () => {
  it("offers both modes, is global-only, and saves { sandbox: { shared_credential } }", async () => {
    const { container } = render(
      <SettingsView onClose={() => {}} tab="sandbox" onSelectTab={() => {}} onServerAboutRefresh={() => {}} />,
    );
    const label = await screen.findByText("Shared agent credential");
    expect(label.parentElement?.textContent).toContain("Applies to all profiles (not profile-overridable).");

    const select = label.parentElement?.querySelector("select") as HTMLSelectElement;
    expect(select).toBeTruthy();
    await waitFor(() => expect(select.value).toBe("host_seed"));
    expect(Array.from(select.options).map((o) => [o.value, o.textContent])).toEqual([
      ["host_seed", "Seed from host login"],
      ["separate_login", "Separate sandbox login"],
    ]);

    fireEvent.change(select, { target: { value: "separate_login" } });
    await waitFor(() =>
      expect(api.updateSettings).toHaveBeenCalledWith({ sandbox: { shared_credential: "separate_login" } }, "main"),
    );
    expect(container.querySelectorAll("select")).toHaveLength(1);
  });
});
