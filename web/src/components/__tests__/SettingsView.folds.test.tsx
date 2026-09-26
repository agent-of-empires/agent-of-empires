// @vitest-environment jsdom
// Settings "Advanced" folds (#1515): collapsed by default, reset on tab or profile switch. The browser
// persist-after-expand path lives in tests/settings-advanced-fold.spec.ts.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [
  { name: "main", is_default: true },
  { name: "work", is_default: false },
];

const ELEV = { policy: "requires_elevation", reason: "host filesystem" } as const;
type Row = [
  section: string,
  field: string,
  label: string,
  widget: Record<string, unknown>,
  advanced: boolean,
  elevated?: boolean,
  rule?: string,
];

// A representative slice of the real `#[setting(...)]` shapes, labels included.
const ROWS: Row[] = [
  ["worktree", "enabled", "Enabled by Default", { kind: "toggle" }, false, true],
  ["worktree", "path_template", "Path Template", { kind: "text" }, false, true],
  ["worktree", "auto_cleanup", "Auto Cleanup", { kind: "toggle" }, false, true],
  ["worktree", "bare_repo_path_template", "Bare Repo Template", { kind: "text" }, true, true],
  ["worktree", "workspace_path_template", "Workspace Path Template", { kind: "text" }, true, true],
  ["worktree", "delete_branch_on_cleanup", "Delete Branch on Cleanup", { kind: "toggle" }, true, true],
  ["worktree", "init_submodules", "Init Submodules", { kind: "toggle" }, true, true],
  ["sandbox", "enabled_by_default", "Sandbox enabled by default", { kind: "toggle" }, false, true],
  ["sandbox", "cpu_limit", "CPU limit", { kind: "optional_text" }, true],
  ["sandbox", "memory_limit", "Memory limit", { kind: "optional_text" }, true, false, "memory_limit"],
  ["sandbox", "custom_instruction", "Custom instruction", { kind: "text", multiline: true }, true],
  ["sandbox", "environment", "Environment variables", { kind: "list" }, true, true, "env_list"],
  ["sandbox", "extra_volumes", "Extra volumes", { kind: "list" }, true, true, "volume_list"],
  ["sandbox", "port_mappings", "Port mappings", { kind: "list" }, true, true, "port_mapping_list"],
  ["sandbox", "volume_ignores", "Volume ignores", { kind: "list" }, true],
  ["acp", "show_tool_durations", "Show tool-call durations", { kind: "toggle" }, false],
  ["acp", "rate_limit_auto_resume", "Auto-resume after rate limit", { kind: "toggle" }, false],
  ["acp", "replay_events", "History cap (events)", { kind: "number", min: 0 }, false],
  ["acp", "max_concurrent_workers", "Max concurrent workers", { kind: "number", min: 1 }, true],
  ["acp", "silent_orphan_grace_secs", "Silent-orphan grace (s)", { kind: "number", min: 0 }, true],
  ["acp", "auto_stop_idle_secs", "Auto-stop idle workers (s)", { kind: "number", min: 0 }, true],
];

const MOCK_SCHEMA = ROWS.map(([section, field, label, widget, advanced, elevated, rule]) => ({
  section,
  field,
  label,
  widget,
  advanced,
  category: section,
  description: "",
  web_write: elevated ? ELEV : { policy: "allow" },
  profile_overridable: true,
  validation: { rule: rule ?? "none" },
}));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(() => Promise.resolve({ acp: {}, sandbox: {}, worktree: {} })),
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
    <SettingsView onClose={() => {}} tab={tab} onSelectTab={onSelectTab} onServerAboutRefresh={() => {}} />,
  );
  return { ...utils, onSelectTab };
}

function expandAdvanced(container: HTMLElement) {
  const trigger = container.querySelector("button[aria-expanded]") as HTMLButtonElement;
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
  const selector = type === "text" ? 'input[type="text"], textarea' : `input[type="${type}"]`;
  const input = match?.parentElement?.querySelector(selector);
  expect(input).toBeTruthy();
  return input as HTMLInputElement | HTMLTextAreaElement;
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
  const labelEl = Array.from(container.querySelectorAll("label")).find((l) => l.textContent === label);
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
    expect(screen.getByText("History cap (events)")).toBeTruthy();

    // Advanced knobs are absent while collapsed.
    expect(screen.queryByText("Max concurrent workers")).toBeNull();
    expect(screen.queryByText("Silent-orphan grace (s)")).toBeNull();

    expandAdvanced(container);

    expect(screen.getByText("Max concurrent workers")).toBeTruthy();
    expect(screen.getByText("Silent-orphan grace (s)")).toBeTruthy();
  });

  it("collapses the fold when switching tabs, with no cross-tab leak (#4)", async () => {
    const { container, rerender } = renderView("sandbox");
    await screen.findByText("Sandbox enabled by default");

    expandAdvanced(container);
    expect(screen.getByText("CPU limit")).toBeTruthy();

    // Switch to worktree: its Advanced fold starts collapsed (no leaked
    // open-state from the sandbox tab sharing the same root element).
    rerender(<SettingsView onClose={() => {}} tab="worktree" onSelectTab={() => {}} onServerAboutRefresh={() => {}} />);
    await screen.findByText("Enabled by Default");
    expect(screen.queryByText("Bare Repo Template")).toBeNull();

    // Back to sandbox: the fold reset to collapsed.
    rerender(<SettingsView onClose={() => {}} tab="sandbox" onSelectTab={() => {}} onServerAboutRefresh={() => {}} />);
    await screen.findByText("Sandbox enabled by default");
    expect(screen.queryByText("CPU limit")).toBeNull();
  });

  it("saves structured-view controls inside and outside the fold through the normal path", async () => {
    const { container } = renderView("structured-view");
    await screen.findByText("Show tool-call durations");

    expandAdvanced(container);
    commit(fieldInputByLabel(container, "Max concurrent workers", "number"), "50");
    commit(fieldInputByLabel(container, "Silent-orphan grace (s)", "number"), "240");
    commit(fieldInputByLabel(container, "Auto-stop idle workers (s)", "number"), "28800");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        acp: { max_concurrent_workers: 50 },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      acp: { silent_orphan_grace_secs: 240 },
    });
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      acp: { auto_stop_idle_secs: 28800 },
    });

    // High-level controls outside the fold save the same way.
    commit(fieldInputByLabel(container, "History cap (events)", "number"), "500");
    clickToggle(container, "Auto-resume after rate limit");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        acp: { replay_events: 500 },
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

    commit(fieldInputByLabel(container, "Bare Repo Template", "text"), "./{branch}");
    commit(fieldInputByLabel(container, "Workspace Path Template", "text"), "../wt-{branch}");
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

  // Regression: the mount-time fetchProfiles resolution flips selectedProfile from its "" seed to the default.
  it("keeps an expanded fold open when the initial profile resolves", async () => {
    let resolveProfiles!: (p: typeof PROFILES) => void;
    vi.mocked(api.fetchProfiles).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveProfiles = resolve;
        }),
    );

    const { container } = renderView("structured-view");
    await screen.findByText("Show tool-call durations");

    expandAdvanced(container);
    expect(screen.getByText("Silent-orphan grace (s)")).toBeTruthy();

    // Profiles resolve: selectedProfile flips "" -> "main". Pre-fix this
    // remounted the fieldset and collapsed the fold.
    await act(async () => {
      resolveProfiles(PROFILES);
    });

    await waitFor(() => expect(vi.mocked(api.fetchSettings)).toHaveBeenCalledWith("main"));
    expect(screen.getByText("Silent-orphan grace (s)")).toBeTruthy();
  });

  it("collapses the fold when switching profiles (#4)", async () => {
    const { container } = renderView("structured-view");
    await screen.findByText("Show tool-call durations");

    expandAdvanced(container);
    expect(screen.getByText("Silent-orphan grace (s)")).toBeTruthy();

    selectProfile(container, "work");

    await waitFor(() => expect(screen.queryByText("Silent-orphan grace (s)")).toBeNull());
  });
});
