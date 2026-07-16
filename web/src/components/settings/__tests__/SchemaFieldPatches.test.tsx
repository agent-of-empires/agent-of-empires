// @vitest-environment jsdom
//
// Payload-permutation coverage for the schema-driven settings tabs, ported
// from the live Playwright story specs (settings-tmux-select, settings-tmux-
// mouse, settings-logging-level, settings-snooze-duration, settings-sound-
// toggle, plus the UI half of settings-persistence-tmux):
//
//   - change the tmux status_bar / mouse selects and the change reaches the
//     selected profile as { tmux: { status_bar | mouse: ... } }
//   - change the logging default level select and it lands as
//     { logging: { default_level: ... } }
//   - commit a new snooze duration and it lands as
//     { session: { snooze_duration_minutes: <number> } }
//   - flip the sound Enabled toggle and it lands as { sound: { enabled: true } }
//
// Each tab is a SchemaSection fed by `GET /api/settings/schema`, so the mock
// schema below mirrors the real `#[setting(...)]` shapes (labels, widgets,
// options) from src/session/config.rs / src/sound/config.rs. What this pins is
// the exact (profile, { section: { field: value } }) PATCH leaf each control
// emits; server-side persistence of the PATCH is the server's contract, not
// the dashboard's.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../../SettingsView";
import * as api from "../../../lib/api";

const PROFILES = [{ name: "main", is_default: true }];

const ALLOW = { policy: "allow" } as const;
const NONE = { rule: "none" } as const;

const TMUX_MODES = [
  { value: "auto", label: "Auto" },
  { value: "enabled", label: "Enabled" },
  { value: "disabled", label: "Disabled" },
];

const SCHEMA = [
  {
    section: "tmux",
    field: "status_bar",
    category: "Tmux",
    label: "Status Bar",
    description: "",
    widget: { kind: "select", options: TMUX_MODES },
    web_write: ALLOW,
    profile_overridable: true,
    validation: NONE,
    advanced: false,
  },
  {
    section: "tmux",
    field: "mouse",
    category: "Tmux",
    label: "Mouse Support",
    description: "",
    widget: { kind: "select", options: TMUX_MODES },
    web_write: ALLOW,
    profile_overridable: true,
    validation: NONE,
    advanced: false,
  },
  {
    section: "logging",
    field: "default_level",
    category: "Logging",
    label: "Default level",
    description: "",
    widget: {
      kind: "select",
      options: ["trace", "debug", "info", "warn", "error"].map((v) => ({ value: v, label: v })),
    },
    web_write: ALLOW,
    // global_only in the real schema: shown but not profile-overridable.
    profile_overridable: false,
    validation: NONE,
    advanced: false,
  },
  {
    section: "session",
    field: "snooze_duration_minutes",
    category: "Session",
    label: "Snooze Duration (minutes)",
    description: "",
    widget: { kind: "number", min: 1, max: 43200 },
    web_write: ALLOW,
    profile_overridable: true,
    validation: { rule: "range", min: 1, max: 43200 },
    advanced: false,
  },
  {
    section: "sound",
    field: "enabled",
    category: "Sound",
    label: "Enabled",
    description: "Play sounds on agent state transitions.",
    widget: { kind: "toggle" },
    web_write: ALLOW,
    profile_overridable: true,
    validation: NONE,
    advanced: false,
  },
];

vi.mock("../../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() => Promise.resolve({ tmux: {}, logging: {}, session: {}, sound: {} })),
  getSettingsSchema: vi.fn(() => Promise.resolve(SCHEMA)),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  updateTheme: vi.fn(() => Promise.resolve(true)),
  fetchThemes: vi.fn(() => Promise.resolve([])),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

function renderTab(tab: string) {
  return render(<SettingsView onClose={() => {}} tab={tab} onSelectTab={() => {}} onServerAboutRefresh={() => {}} />);
}

/** The <select> rendered next to a unique field label. Labels in FormFields
 *  are not wired to their controls, so walk from the label element. */
function selectByLabel(container: HTMLElement, label: string): HTMLSelectElement {
  const match = Array.from(container.querySelectorAll("label")).find((l) => l.textContent === label);
  const select = match?.parentElement?.querySelector("select");
  expect(select).toBeTruthy();
  return select as HTMLSelectElement;
}

function numberInputByLabel(container: HTMLElement, label: string): HTMLInputElement {
  const match = Array.from(container.querySelectorAll("label")).find((l) => l.textContent === label);
  const input = match?.parentElement?.querySelector('input[type="number"]');
  expect(input).toBeTruthy();
  return input as HTMLInputElement;
}

// NumberField re-syncs from its prop unless focused, so focus before typing;
// it commits on blur.
function commit(input: HTMLInputElement, value: string) {
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

describe("schema-driven settings field PATCH payloads", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("tmux Status Bar select emits { tmux: { status_bar } } to the selected profile", async () => {
    const { container } = renderTab("tmux");
    await screen.findByText("Status Bar");

    fireEvent.change(selectByLabel(container, "Status Bar"), {
      target: { value: "disabled" },
    });

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        tmux: { status_bar: "disabled" },
      }),
    );
  });

  it("tmux Mouse Support select emits { tmux: { mouse } }", async () => {
    const { container } = renderTab("tmux");
    await screen.findByText("Mouse Support");

    fireEvent.change(selectByLabel(container, "Mouse Support"), {
      target: { value: "disabled" },
    });

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        tmux: { mouse: "disabled" },
      }),
    );
  });

  it("a tmux field edit never leaks sibling fields into the PATCH (sparse leaf)", async () => {
    // The live tmux persistence spec PATCHed both fields at once; the UI
    // contract is the opposite: each control writes only its own leaf so a
    // concurrent edit on another surface is never clobbered.
    vi.mocked(api.fetchSettings).mockResolvedValueOnce({
      tmux: { status_bar: "enabled", mouse: "enabled" },
      logging: {},
      session: {},
      sound: {},
    } as never);
    const { container } = renderTab("tmux");
    await screen.findByText("Status Bar");
    await waitFor(() => expect(selectByLabel(container, "Status Bar").value).toBe("enabled"));

    fireEvent.change(selectByLabel(container, "Status Bar"), {
      target: { value: "disabled" },
    });

    await waitFor(() => expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalled());
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      tmux: { status_bar: "disabled" },
    });
    // No call carries the untouched `mouse` field.
    for (const [, updates] of vi.mocked(api.updateProfileSettings).mock.calls) {
      expect((updates as { tmux?: Record<string, unknown> }).tmux).not.toHaveProperty("mouse");
    }
  });

  it("logging Default level select emits { logging: { default_level } }", async () => {
    const { container } = renderTab("logging");
    await screen.findByText("Default level");

    fireEvent.change(selectByLabel(container, "Default level"), {
      target: { value: "debug" },
    });

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        logging: { default_level: "debug" },
      }),
    );
  });

  it("session Snooze Duration commit emits { session: { snooze_duration_minutes } } as a number", async () => {
    const { container } = renderTab("session");
    await screen.findByText("Snooze Duration (minutes)");

    commit(numberInputByLabel(container, "Snooze Duration (minutes)"), "12");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        session: { snooze_duration_minutes: 12 },
      }),
    );
  });

  it("sound Enabled toggle emits { sound: { enabled: true } }", async () => {
    const { container } = renderTab("sound");
    await screen.findByText("Enabled");

    clickToggle(container, "Enabled");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        sound: { enabled: true },
      }),
    );
  });
});
