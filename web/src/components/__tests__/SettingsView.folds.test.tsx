// @vitest-environment jsdom
//
// Behavioral coverage for the Settings "Advanced" folds (#1515):
//   Story #2 - advanced knobs are hidden behind a default-collapsed fold while
//              high-level controls stay visible.
//   Story #4 - the fold collapses back to default when the user changes tabs
//              or switches profiles (component-local state, not persisted).
//
// Every config-backed section is schema-driven (#1692): SchemaSection builds
// its rows (and the advanced fold) from the descriptor list below, so this
// mock mirrors the real worktree / sandbox / acp `#[setting(...)]` shapes. The
// end-to-end persist-after-expand path (story #3) lives in live Playwright at
// web/tests/live/settings-advanced-fold.spec.ts.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [
  { name: "main", is_default: true },
  { name: "work", is_default: false },
];

const ALLOW = { policy: "allow" } as const;
const ELEV = { policy: "requires_elevation", reason: "host filesystem" } as const;
const NONE = { rule: "none" } as const;

// Representative slice of the real schema: a few primary + advanced fields per
// section, enough to exercise the fold across sections without mirroring every
// field. Labels match the real `#[setting(label = ...)]` values.
const RAW_SCHEMA: Array<{
  section: string;
  field: string;
  label: string;
  widget: Record<string, unknown>;
  advanced: boolean;
  web_write?: typeof ALLOW | typeof ELEV;
  validation?: Record<string, unknown>;
}> = [
  // worktree
  { section: "worktree", field: "enabled", label: "Enabled by Default", widget: { kind: "toggle" }, advanced: false, web_write: ELEV },
  { section: "worktree", field: "path_template", label: "Path Template", widget: { kind: "text" }, advanced: false, web_write: ELEV },
  { section: "worktree", field: "auto_cleanup", label: "Auto Cleanup", widget: { kind: "toggle" }, advanced: false, web_write: ELEV },
  { section: "worktree", field: "bare_repo_path_template", label: "Bare Repo Template", widget: { kind: "text" }, advanced: true, web_write: ELEV },
  { section: "worktree", field: "workspace_path_template", label: "Workspace Path Template", widget: { kind: "text" }, advanced: true, web_write: ELEV },
  { section: "worktree", field: "delete_branch_on_cleanup", label: "Delete Branch on Cleanup", widget: { kind: "toggle" }, advanced: true, web_write: ELEV },
  { section: "worktree", field: "init_submodules", label: "Init Submodules", widget: { kind: "toggle" }, advanced: true, web_write: ELEV },
  // sandbox
  { section: "sandbox", field: "enabled_by_default", label: "Sandbox enabled by default", widget: { kind: "toggle" }, advanced: false, web_write: ELEV },
  { section: "sandbox", field: "cpu_limit", label: "CPU limit", widget: { kind: "optional_text" }, advanced: true },
  { section: "sandbox", field: "memory_limit", label: "Memory limit", widget: { kind: "optional_text" }, advanced: true, validation: { rule: "memory_limit" } },
  { section: "sandbox", field: "custom_instruction", label: "Custom instruction", widget: { kind: "text", multiline: true }, advanced: true },
  { section: "sandbox", field: "environment", label: "Environment variables", widget: { kind: "list" }, advanced: true, web_write: ELEV, validation: { rule: "env_list" } },
  { section: "sandbox", field: "extra_volumes", label: "Extra volumes", widget: { kind: "list" }, advanced: true, web_write: ELEV, validation: { rule: "volume_list" } },
  { section: "sandbox", field: "port_mappings", label: "Port mappings", widget: { kind: "list" }, advanced: true, web_write: ELEV, validation: { rule: "port_mapping_list" } },
  { section: "sandbox", field: "volume_ignores", label: "Volume ignores", widget: { kind: "list" }, advanced: true },
  // acp (structured view)
  { section: "acp", field: "show_tool_durations", label: "Show tool-call durations", widget: { kind: "toggle" }, advanced: false },
  { section: "acp", field: "queue_drain_mode", label: "Queue drain mode", widget: { kind: "select", options: [ { value: "combined", label: "Combined" }, { value: "serial", label: "Serial" } ] }, advanced: false },
  { section: "acp", field: "rate_limit_auto_resume", label: "Auto-resume after rate limit", widget: { kind: "toggle" }, advanced: false },
  { section: "acp", field: "replay_events", label: "History cap (events)", widget: { kind: "number", min: 0 }, advanced: true },
  { section: "acp", field: "replay_bytes", label: "Replay buffer bytes", widget: { kind: "number", min: 0 }, advanced: true },
  { section: "acp", field: "max_concurrent_resumes", label: "Max concurrent resumes", widget: { kind: "number", min: 1 }, advanced: true },
  { section: "acp", field: "silent_orphan_grace_secs", label: "Silent-orphan grace (s)", widget: { kind: "number", min: 0 }, advanced: true },
  { section: "acp", field: "silent_orphan_fast_grace_secs", label: "Silent-orphan fast grace (s)", widget: { kind: "number", min: 0 }, advanced: true },
  { section: "acp", field: "auto_stop_idle_secs", label: "Auto-stop idle workers (s)", widget: { kind: "number", min: 0 }, advanced: true },
  { section: "acp", field: "rate_limit_auto_resume_grace_secs", label: "Auto-resume grace (s)", widget: { kind: "number", min: 0 }, advanced: true },
];

const MOCK_SCHEMA = RAW_SCHEMA.map((d) => ({
  category: d.section,
  description: "",
  web_write: d.web_write ?? ALLOW,
  profile_overridable: true,
  validation: d.validation ?? NONE,
  ...d,
}));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() =>
    Promise.resolve({ acp: {}, sandbox: {}, worktree: {} }),
  ),
  getSettingsSchema: vi.fn(() => Promise.resolve(MOCK_SCHEMA)),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

function renderView(tab: string) {
  const onSelectTab = vi.fn();
  const utils = render(
    <SettingsView
      onClose={() => {}}
      tab={tab}
      onSelectTab={onSelectTab}
      onServerAboutRefresh={() => {}}
    />,
  );
  return { ...utils, onSelectTab };
}

function expandAdvanced(container: HTMLElement) {
  const trigger = container.querySelector(
    "button[aria-expanded]",
  ) as HTMLButtonElement;
  expect(trigger).toBeTruthy();
  fireEvent.click(trigger);
}

function fieldInputByLabel(
  container: HTMLElement,
  label: string,
  type: "number" | "text",
): HTMLInputElement | HTMLTextAreaElement {
  const labels = Array.from(container.querySelectorAll("label"));
  const match = labels.find((l) => l.textContent === label);
  const selector =
    type === "text" ? 'input[type="text"], textarea' : `input[type="${type}"]`;
  const input = match?.parentElement?.querySelector(selector);
  expect(input).toBeTruthy();
  return input as HTMLInputElement | HTMLTextAreaElement;
}

function selectByLabel(container: HTMLElement, label: string): HTMLSelectElement {
  const labels = Array.from(container.querySelectorAll("label"));
  const match = labels.find((l) => l.textContent === label);
  const select = match?.parentElement?.querySelector("select");
  expect(select).toBeTruthy();
  return select as HTMLSelectElement;
}

function commit(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

// ToggleField renders a label div next to a role=switch button inside a flex
// row; click the switch that pairs with the given label.
function clickToggle(container: HTMLElement, label: string) {
  const labelDiv = Array.from(container.querySelectorAll("div")).find(
    (d) => d.textContent === label && d.querySelector("*") === null,
  );
  const row = labelDiv?.parentElement?.parentElement;
  const sw = row?.querySelector('button[role="switch"]') as HTMLButtonElement;
  expect(sw).toBeTruthy();
  fireEvent.click(sw);
}

// ListField: open its add input, type a value, submit with Enter. Scoped to
// the ListField whose header carries `label`.
function addListItem(container: HTMLElement, label: string, value: string) {
  const labelEl = Array.from(container.querySelectorAll("label")).find(
    (l) => l.textContent === label,
  );
  const root = labelEl?.parentElement?.parentElement as HTMLElement;
  const addBtn = labelEl?.parentElement?.querySelector("button");
  if (addBtn) fireEvent.click(addBtn);
  const input = root.querySelector('input[type="text"]') as HTMLInputElement;
  fireEvent.change(input, { target: { value } });
  fireEvent.keyDown(input, { key: "Enter" });
}

// The profile picker is the only <select> carrying the "work" option.
function selectProfile(container: HTMLElement, name: string) {
  const select = Array.from(container.querySelectorAll("select")).find((s) =>
    Array.from(s.options).some((o) => o.value === name),
  ) as HTMLSelectElement;
  expect(select).toBeTruthy();
  fireEvent.change(select, { target: { value: name } });
}

describe("Settings Advanced fold", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("hides structured-view advanced knobs until the fold is expanded (#2)", async () => {
    const { container } = renderView("structured-view");
    await screen.findByText("Show tool-call durations");

    // High-level controls are always visible.
    expect(screen.getByText("Show tool-call durations")).toBeTruthy();
    expect(screen.getByText("Queue drain mode")).toBeTruthy();

    // Advanced knobs are absent while collapsed.
    expect(screen.queryByText("Replay buffer bytes")).toBeNull();
    expect(screen.queryByText("Max concurrent resumes")).toBeNull();
    expect(screen.queryByText("Silent-orphan grace (s)")).toBeNull();

    expandAdvanced(container);

    expect(screen.getByText("Replay buffer bytes")).toBeTruthy();
    expect(screen.getByText("Max concurrent resumes")).toBeTruthy();
    expect(screen.getByText("Silent-orphan grace (s)")).toBeTruthy();
  });

  it("collapses the fold when switching tabs, with no cross-tab leak (#4)", async () => {
    const { container, rerender } = renderView("sandbox");
    await screen.findByText("Sandbox enabled by default");

    expandAdvanced(container);
    expect(screen.getByText("CPU limit")).toBeTruthy();

    // Switch to worktree: its Advanced fold starts collapsed (no leaked
    // open-state from the sandbox tab sharing the same root element).
    rerender(
      <SettingsView
        onClose={() => {}}
        tab="worktree"
        onSelectTab={() => {}}
        onServerAboutRefresh={() => {}}
      />,
    );
    await screen.findByText("Enabled by Default");
    expect(screen.queryByText("Bare Repo Template")).toBeNull();

    // Back to sandbox: the fold reset to collapsed.
    rerender(
      <SettingsView
        onClose={() => {}}
        tab="sandbox"
        onSelectTab={() => {}}
        onServerAboutRefresh={() => {}}
      />,
    );
    await screen.findByText("Sandbox enabled by default");
    expect(screen.queryByText("CPU limit")).toBeNull();
  });

  it("saves structured-view advanced knobs through the normal path", async () => {
    const { container } = renderView("structured-view");
    await screen.findByText("Queue drain mode");

    expandAdvanced(container);
    commit(fieldInputByLabel(container, "Replay buffer bytes", "number"), "4096");
    commit(
      fieldInputByLabel(container, "Silent-orphan fast grace (s)", "number"),
      "30",
    );
    commit(
      fieldInputByLabel(container, "Auto-stop idle workers (s)", "number"),
      "28800",
    );
    commit(fieldInputByLabel(container, "Auto-resume grace (s)", "number"), "20");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        acp: { replay_bytes: 4096 },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      acp: { silent_orphan_fast_grace_secs: 30 },
    });
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      acp: { auto_stop_idle_secs: 28800 },
    });
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      acp: { rate_limit_auto_resume_grace_secs: 20 },
    });
  });

  it("exercises the structured-view high-level controls outside the fold", async () => {
    const { container } = renderView("structured-view");
    await screen.findByText("Queue drain mode");

    fireEvent.change(selectByLabel(container, "Queue drain mode"), {
      target: { value: "serial" },
    });
    clickToggle(container, "Auto-resume after rate limit");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        acp: { queue_drain_mode: "serial" },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      acp: { rate_limit_auto_resume: true },
    });
  });

  it("expands the worktree fold and saves every advanced field", async () => {
    const { container } = renderView("worktree");
    await screen.findByText("Enabled by Default");

    expect(screen.queryByText("Workspace Path Template")).toBeNull();
    expandAdvanced(container);
    expect(screen.getByText("Workspace Path Template")).toBeTruthy();

    commit(
      fieldInputByLabel(container, "Bare Repo Template", "text"),
      "./{branch}",
    );
    commit(
      fieldInputByLabel(container, "Workspace Path Template", "text"),
      "../wt-{branch}",
    );
    clickToggle(container, "Delete Branch on Cleanup");
    clickToggle(container, "Init Submodules");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        worktree: { workspace_path_template: "../wt-{branch}" },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      worktree: { delete_branch_on_cleanup: true },
    });
  });

  it("saves sandbox advanced fields, including the derived list validators", async () => {
    const { container } = renderView("sandbox");
    await screen.findByText("Sandbox enabled by default");

    expandAdvanced(container);
    commit(fieldInputByLabel(container, "CPU limit", "text"), "4");
    commit(fieldInputByLabel(container, "Memory limit", "text"), "8g");
    commit(fieldInputByLabel(container, "Custom instruction", "text"), "be terse");

    // Lists exercise both the add (onChange) and validate paths: an invalid
    // entry trips the schema-derived validator, then a valid one commits.
    addListItem(container, "Environment variables", "1bad");
    addListItem(container, "Environment variables", "FOO=bar");
    addListItem(container, "Extra volumes", "nocolon");
    addListItem(container, "Extra volumes", "/h:/c");
    addListItem(container, "Port mappings", "bad");
    addListItem(container, "Port mappings", "3000:3000");
    addListItem(container, "Volume ignores", "node_modules");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        sandbox: { cpu_limit: "4" },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      sandbox: { environment: ["FOO=bar"] },
    });
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      sandbox: { port_mappings: ["3000:3000"] },
    });
  });

  // Regression: the mount-time fetchProfiles resolution flips selectedProfile
  // from its "" seed to the default. That transition must NOT remount the
  // content fieldset, or a fold expanded during the load window collapses out
  // from under the user (the deterministic mirror of the live flake in
  // tests/live/settings-advanced-fold.spec.ts).
  it("keeps an expanded fold open when the initial profile resolves", async () => {
    let resolveProfiles!: (p: typeof PROFILES) => void;
    vi.mocked(api.fetchProfiles).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveProfiles = resolve;
        }),
    );

    const { container } = renderView("structured-view");
    await screen.findByText("Queue drain mode");

    expandAdvanced(container);
    expect(screen.getByText("Replay buffer bytes")).toBeTruthy();

    // Profiles resolve: selectedProfile flips "" -> "main". Pre-fix this
    // remounted the fieldset and collapsed the fold.
    await act(async () => {
      resolveProfiles(PROFILES);
    });

    await waitFor(() =>
      expect(vi.mocked(api.fetchSettings)).toHaveBeenCalledWith("main"),
    );
    expect(screen.getByText("Replay buffer bytes")).toBeTruthy();
  });

  it("collapses the fold when switching profiles (#4)", async () => {
    const { container } = renderView("structured-view");
    await screen.findByText("Queue drain mode");

    expandAdvanced(container);
    expect(screen.getByText("Replay buffer bytes")).toBeTruthy();

    selectProfile(container, "work");

    await waitFor(() =>
      expect(screen.queryByText("Replay buffer bytes")).toBeNull(),
    );
  });
});
