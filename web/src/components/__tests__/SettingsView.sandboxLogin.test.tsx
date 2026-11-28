// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import { resetSettingsStore } from "../../lib/settingsStore";

const SCHEMA = [
  {
    section: "sandbox",
    field: "shared_credential",
    label: "shared_credential",
    widget: { kind: "toggle" },
    advanced: false,
    category: "sandbox",
    description: "",
    web_write: { policy: "allow" },
    profile_overridable: false,
    validation: { rule: "none" },
  },
];

const api = vi.hoisted(() => ({
  fetchSettings: vi.fn(),
  updateSettings: vi.fn(),
  getSandboxLoginStatus: vi.fn(),
}));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve([{ name: "main", is_default: true }])),
  fetchSuggestions: vi.fn(() => Promise.resolve(null)),
  getSandboxLoginStatus: api.getSandboxLoginStatus,
  fetchSettings: api.fetchSettings,
  getSettingsSchema: vi.fn(() => Promise.resolve(SCHEMA)),
  updateProfileSettings: vi.fn(() => Promise.resolve("committed" as const)),
  updateSettings: api.updateSettings,
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
}));

beforeEach(() => {
  resetSettingsStore();
  api.getSandboxLoginStatus.mockResolvedValue({ state: "needed", running: false, heldSessions: 0 });
  api.fetchSettings.mockResolvedValue({ sandbox: { shared_credential: false } });
  api.updateSettings.mockResolvedValue("committed");
});
afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const view = (readOnly = false) => (
  <SettingsView
    onClose={() => {}}
    tab="sandbox"
    onSelectTab={() => {}}
    onServerAboutRefresh={() => {}}
    readOnly={readOnly}
  />
);

describe("SettingsView sandbox login row", () => {
  it("shows no Log in button on a read-only server", async () => {
    render(view(true));
    await screen.findByText("Sandbox login: needed");
    expect(screen.queryByRole("button", { name: /^Log in/ })).toBeNull();
  });

  it("re-reads the login status once an uncertain credential save is confirmed", async () => {
    api.updateSettings.mockResolvedValue("uncertain");
    render(view());
    await screen.findByText("Sandbox login: needed");

    // The server only shows the new mode, and the login its new state, once the reconcile fetch lands.
    let confirmed = false;
    api.getSandboxLoginStatus.mockImplementation(() =>
      Promise.resolve({ state: confirmed ? "ok" : "needed", running: false, heldSessions: 0 }),
    );
    let confirm: (() => void) | undefined;
    api.fetchSettings.mockImplementation(
      () =>
        new Promise((resolve) => {
          confirm = () => {
            confirmed = true;
            resolve({ sandbox: { shared_credential: true } });
          };
        }),
    );
    const reads = api.getSandboxLoginStatus.mock.calls.length;
    fireEvent.click(await screen.findByRole("switch"));
    // The save's own answer re-reads the status while the server has not confirmed the mode yet.
    await waitFor(() => expect(api.getSandboxLoginStatus.mock.calls.length).toBeGreaterThan(reads));
    await waitFor(() => expect(confirm).toBeDefined());
    confirm?.();
    await screen.findByText("Sandbox login: ok");
  });
});
