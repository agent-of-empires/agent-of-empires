// @vitest-environment jsdom
//
// Behavioral coverage for the Settings "Advanced" folds (#1515):
//   Story #2 - advanced cockpit knobs are hidden behind a default-collapsed
//              fold while high-level controls stay visible.
//   Story #4 - the fold collapses back to default when the user changes tabs
//              or switches profiles (component-local state, not persisted).
//
// The end-to-end persist-after-expand path (story #3) lives in live Playwright
// at web/tests/live/settings-advanced-fold.spec.ts.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [
  { name: "main", is_default: true },
  { name: "work", is_default: false },
];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() =>
    Promise.resolve({ cockpit: {}, sandbox: {}, worktree: {} }),
  ),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setCockpitMaster: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

const SERVER_ABOUT = {
  cockpit_master_enabled: true,
  cockpit_show_tool_durations: true,
  cockpit_queue_drain_mode: "combined" as const,
  cockpit_max_concurrent_resumes: 4,
};

function renderView(tab: string) {
  const onSelectTab = vi.fn();
  const utils = render(
    <SettingsView
      onClose={() => {}}
      tab={tab}
      onSelectTab={onSelectTab}
      serverAbout={SERVER_ABOUT as never}
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
  // TextField renders a textarea when multiline (e.g. Custom instruction).
  const selector =
    type === "text" ? 'input[type="text"], textarea' : `input[type="${type}"]`;
  const input = match?.parentElement?.querySelector(selector);
  expect(input).toBeTruthy();
  return input as HTMLInputElement | HTMLTextAreaElement;
}

function commit(
  input: HTMLInputElement | HTMLTextAreaElement,
  value: string,
) {
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
// the ListField whose header carries `label` so the right "+ Add" / input pair
// is used when several lists render together.
function addListItem(container: HTMLElement, label: string, value: string) {
  const labelEl = Array.from(container.querySelectorAll("label")).find(
    (l) => l.textContent === label,
  );
  const root = labelEl?.parentElement?.parentElement as HTMLElement;
  // "+ Add" is hidden while the add input is already open (e.g. after a
  // rejected invalid entry); only click it when present, then reuse the input.
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

  it("hides cockpit advanced knobs until the fold is expanded (#2)", async () => {
    const { container } = renderView("cockpit");

    // High-level controls are always visible.
    expect(screen.getByText("Cockpit master switch")).toBeTruthy();
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
        serverAbout={SERVER_ABOUT as never}
        onServerAboutRefresh={() => {}}
      />,
    );
    await screen.findByText("Worktrees enabled");
    expect(screen.queryByText("Bare repo path template")).toBeNull();

    // Back to sandbox: the fold reset to collapsed.
    rerender(
      <SettingsView
        onClose={() => {}}
        tab="sandbox"
        onSelectTab={() => {}}
        serverAbout={SERVER_ABOUT as never}
        onServerAboutRefresh={() => {}}
      />,
    );
    await screen.findByText("Sandbox enabled by default");
    expect(screen.queryByText("CPU limit")).toBeNull();
  });

  it("saves every cockpit advanced knob through the normal path", async () => {
    const { container } = renderView("cockpit");
    await waitFor(() => expect(screen.getByText("Queue drain mode")).toBeTruthy());

    expandAdvanced(container);
    commit(fieldInputByLabel(container, "History cap (events)", "number"), "500");
    commit(fieldInputByLabel(container, "Replay buffer bytes", "number"), "4096");
    commit(fieldInputByLabel(container, "Max concurrent resumes", "number"), "8");
    commit(fieldInputByLabel(container, "Silent-orphan grace (s)", "number"), "90");
    commit(
      fieldInputByLabel(container, "Silent-orphan fast grace (s)", "number"),
      "30",
    );
    commit(
      fieldInputByLabel(container, "Auto-stop idle workers (s)", "number"),
      "28800",
    );

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith(
        "main",
        { cockpit: { replay_bytes: 4096 } },
      ),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      cockpit: { silent_orphan_fast_grace_secs: 30 },
    });
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      cockpit: { auto_stop_idle_secs: 28800 },
    });
  });

  it("exercises the cockpit high-level toggles outside the fold", async () => {
    const { container } = renderView("cockpit");
    await waitFor(() => expect(screen.getByText("Queue drain mode")).toBeTruthy());

    const durations = container.querySelector(
      'button[aria-label="Show tool-call durations"]',
    ) as HTMLButtonElement;
    fireEvent.click(durations);

    const serial = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent === "Serial",
    ) as HTMLButtonElement;
    fireEvent.click(serial);

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        cockpit: { queue_drain_mode: "serial" },
      }),
    );
  });

  it("expands the worktree fold and saves every advanced field", async () => {
    const { container } = renderView("worktree");
    await screen.findByText("Worktrees enabled");

    expect(screen.queryByText("Workspace path template")).toBeNull();
    expandAdvanced(container);
    expect(screen.getByText("Workspace path template")).toBeTruthy();

    commit(
      fieldInputByLabel(container, "Bare repo path template", "text"),
      "./{branch}",
    );
    commit(
      fieldInputByLabel(container, "Workspace path template", "text"),
      "../wt-{branch}",
    );
    clickToggle(container, "Delete branch on cleanup");
    clickToggle(container, "Init submodules");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        worktree: { workspace_path_template: "../wt-{branch}" },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      worktree: { delete_branch_on_cleanup: true },
    });
  });

  it("saves every sandbox advanced field through the normal path", async () => {
    const { container } = renderView("sandbox");
    await screen.findByText("Sandbox enabled by default");

    expandAdvanced(container);
    commit(fieldInputByLabel(container, "CPU limit", "text"), "4");
    commit(fieldInputByLabel(container, "Memory limit", "text"), "8g");
    commit(fieldInputByLabel(container, "Custom instruction", "text"), "be terse");

    // Lists exercise both the add (onChange) and validate paths: an invalid
    // entry trips the validator, then a valid one commits.
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
  });

  // Regression: the mount-time fetchProfiles resolution flips selectedProfile
  // from its "" seed to the default. That transition must NOT remount the
  // content fieldset, or a fold expanded during the load window collapses out
  // from under the user. This is the deterministic mirror of the live flake in
  // tests/live/settings-advanced-fold.spec.ts (the cockpit "Advanced" fold
  // vanishing right after a click).
  it("keeps an expanded fold open when the initial profile resolves", async () => {
    let resolveProfiles!: (p: typeof PROFILES) => void;
    vi.mocked(api.fetchProfiles).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveProfiles = resolve;
        }),
    );

    const { container } = renderView("cockpit");

    // Cockpit renders without waiting on profiles/settings, so the fold is
    // interactive during the load window. Open it before profiles resolve.
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
    const { container } = renderView("cockpit");
    await waitFor(() => expect(screen.getByText("Queue drain mode")).toBeTruthy());

    expandAdvanced(container);
    expect(screen.getByText("Replay buffer bytes")).toBeTruthy();

    selectProfile(container, "work");

    await waitFor(() =>
      expect(screen.queryByText("Replay buffer bytes")).toBeNull(),
    );
  });
});
