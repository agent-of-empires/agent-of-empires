// @vitest-environment jsdom
//
// Behavioral coverage for the Session tab's "Auto-stop idle sessions" number
// field (#1690): a persisted value renders into the field, and committing a
// value persists `session.auto_stop_idle_secs` through the normal
// profile-settings path, the same wiring the TUI uses.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [{ name: "main", is_default: true }];

// The session tab is schema-driven (#1692): auto_stop_idle_secs is a number
// field and acp_defaults is the acp-defaults custom widget, both built from
// these descriptors. The default-profile selector is the only non-schema row.
const SESSION_SCHEMA = [
  {
    section: "session",
    field: "auto_stop_idle_secs",
    category: "Interaction",
    label: "Auto-stop idle sessions (s)",
    description: "",
    widget: { kind: "number", min: 0 },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  },
  {
    section: "acp",
    field: "acp_defaults",
    category: "Acp",
    label: "Structured View Defaults",
    description: "",
    widget: { kind: "custom", id: "acp-defaults" },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  },
  {
    section: "session",
    field: "smart_rename",
    category: "Agents",
    label: "Smart Session Rename",
    description: "",
    widget: { kind: "toggle" },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  },
  {
    section: "session",
    field: "row_tag",
    category: "Sessions",
    label: "Row Tag",
    description: "What to show next to each session title",
    widget: {
      kind: "select",
      options: [
        { value: "none", label: "None" },
        { value: "auto", label: "Auto" },
        { value: "profile", label: "Profile" },
        { value: "sandbox", label: "Sandbox" },
        { value: "branch", label: "Branch" },
      ],
    },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  },
];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() => Promise.resolve({ session: {}, acp: {}, sandbox: {}, worktree: {} })),
  getSettingsSchema: vi.fn(() => Promise.resolve(SESSION_SCHEMA)),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
  // The rebuilt acp-defaults widget fetches the agent list and option catalog
  // (#2631); the raw-JSON fold it exposes does not depend on either.
  fetchAgents: vi.fn(() => Promise.resolve([])),
  fetchAcpOptionCatalog: vi.fn(() => Promise.resolve({ version: 1, agents: {} })),
}));

function numberInputByLabel(container: HTMLElement, label: string): HTMLInputElement {
  const labels = Array.from(container.querySelectorAll("label"));
  const match = labels.find((l) => l.textContent === label);
  const input = match?.parentElement?.querySelector('input[type="number"]');
  expect(input).toBeTruthy();
  return input as HTMLInputElement;
}

function commit(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

function commitTextarea(input: HTMLTextAreaElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

describe("Session tab auto-stop idle field", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // clearAllMocks resets call history but not implementations, so restore
    // the empty-settings default here to isolate tests that override it.
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: {},
      acp: {},
      sandbox: {},
      worktree: {},
    } as never);
  });

  it("renders the persisted auto_stop_idle_secs value into the field", async () => {
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: { auto_stop_idle_secs: 1800 },
      acp: {},
      sandbox: {},
      worktree: {},
    } as never);

    const { container } = render(
      <SettingsView onClose={() => {}} tab="session" onSelectTab={() => {}} onServerAboutRefresh={() => {}} />,
    );
    await screen.findByText("Auto-stop idle sessions (s)");

    await waitFor(() => expect(numberInputByLabel(container, "Auto-stop idle sessions (s)").value).toBe("1800"));
  });

  it("persists session.auto_stop_idle_secs through the profile path", async () => {
    const { container } = render(
      <SettingsView onClose={() => {}} tab="session" onSelectTab={() => {}} onServerAboutRefresh={() => {}} />,
    );
    await screen.findByText("Auto-stop idle sessions (s)");

    commit(numberInputByLabel(container, "Auto-stop idle sessions (s)"), "7200");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        session: { auto_stop_idle_secs: 7200 },
      }),
    );
  });

  it("persists session.smart_rename through the profile path", async () => {
    const onSettingsRefresh = vi.fn();
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: { smart_rename: true },
      acp: {},
      sandbox: {},
      worktree: {},
    } as never);

    const { container } = render(
      <SettingsView
        onClose={() => {}}
        tab="session"
        onSelectTab={() => {}}
        onServerAboutRefresh={() => {}}
        onSettingsRefresh={onSettingsRefresh}
      />,
    );
    await screen.findByText("Smart Session Rename");

    // Only one toggle field is in SESSION_SCHEMA, so the lone switch is it.
    const toggle = container.querySelector("button[role=switch]") as HTMLButtonElement;
    expect(toggle).toBeTruthy();
    fireEvent.click(toggle);

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        session: { smart_rename: false },
      }),
    );
    expect(onSettingsRefresh).not.toHaveBeenCalled();
  });

  it("refreshes app-level settings after saving session.row_tag", async () => {
    const onSettingsRefresh = vi.fn();
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: { row_tag: "branch" },
      acp: {},
      sandbox: {},
      worktree: {},
    } as never);

    const { container } = render(
      <SettingsView
        onClose={() => {}}
        tab="session"
        onSelectTab={() => {}}
        onServerAboutRefresh={() => {}}
        onSettingsRefresh={onSettingsRefresh}
      />,
    );
    await screen.findByText("Row Tag");

    const selects = Array.from(container.querySelectorAll("select"));
    const rowTagSelect = selects.find((select) =>
      Array.from(select.options).some((option) => option.value === "sandbox"),
    ) as HTMLSelectElement | undefined;
    expect(rowTagSelect).toBeTruthy();
    fireEvent.change(rowTagSelect!, { target: { value: "none" } });

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        session: { row_tag: "none" },
      }),
    );
    expect(onSettingsRefresh).toHaveBeenCalledTimes(1);
  });

  it("persists acp.acp_defaults through the profile path via the raw-JSON fold", async () => {
    // acp_defaults lives on the acp section, so it renders under the Structured
    // view tab, not Session (#2631).
    const { container } = render(
      <SettingsView onClose={() => {}} tab="structured-view" onSelectTab={() => {}} onServerAboutRefresh={() => {}} />,
    );
    await screen.findByText("Structured View Defaults");

    // The rebuilt widget renders per-agent cards; its raw-JSON escape hatch is
    // behind an advanced fold. Open it, then commit the map, same wiring the
    // TUI uses.
    fireEvent.click(screen.getByText("Advanced: edit raw JSON"));
    const textarea = container.querySelector("textarea");
    expect(textarea).toBeTruthy();
    commitTextarea(textarea as HTMLTextAreaElement, '{"opencode":{"model":"openai/gpt-5.5","effort":"high"}}');

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        acp: {
          acp_defaults: {
            opencode: { model: "openai/gpt-5.5", effort: "high" },
          },
        },
      }),
    );
  });
});
