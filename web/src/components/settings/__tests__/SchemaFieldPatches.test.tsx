// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { SettingsView } from "../../SettingsView";
import * as api from "../../../lib/api";
import { descriptor } from "./fixtures";
import type { SettingsFieldDescriptor } from "../../../lib/types";

const PROFILES = [{ name: "main", is_default: true }];

const TMUX_MODES = [
  { value: "auto", label: "Auto" },
  { value: "enabled", label: "Enabled" },
  { value: "disabled", label: "Disabled" },
];

const field = (
  section: string,
  name: string,
  label: string,
  widget: SettingsFieldDescriptor["widget"],
  extra: Partial<SettingsFieldDescriptor> = {},
) => descriptor({ section, field: name, category: section, label, widget, ...extra });

const SCHEMA = [
  field(
    "session",
    "sidebar_position",
    "Sidebar Position",
    {
      kind: "select",
      options: [
        { value: "left", label: "Left" },
        { value: "right", label: "Right" },
      ],
    },
    { profile_overridable: false },
  ),
  ...[
    ["split_terminal_height_percent", "Paired terminal share of the split (%)", 1, 99],
    ["split_terminal_min_width", "Split only when at least this wide", 0, 1000],
    ["split_terminal_min_height", "Split only when at least this tall", 0, 1000],
  ].map(([name, label, min, max]) =>
    field(
      "session",
      name as string,
      label as string,
      { kind: "number", min: min as number, max: max as number },
      {
        tui_only: true,
        profile_overridable: false,
      },
    ),
  ),
  field("tmux", "status_bar", "Status Bar", { kind: "select", options: TMUX_MODES }),
  field("tmux", "mouse", "Mouse Support", { kind: "select", options: TMUX_MODES }),
  field(
    "logging",
    "default_level",
    "Default level",
    { kind: "select", options: ["trace", "debug", "info", "warn", "error"].map((v) => ({ value: v, label: v })) },
    { profile_overridable: false },
  ),
  field("session", "snooze_duration_minutes", "Snooze Duration (minutes)", { kind: "number", min: 1, max: 43200 }),
  field(
    "session",
    "session_id_poller_max_threads",
    "Session-id poller threads",
    { kind: "number", min: 0 },
    {
      profile_overridable: false,
      advanced: true,
      restart: true,
      description: "Ceiling on concurrent session-id poller threads.",
    },
  ),
  field("sound", "enabled", "Enabled", { kind: "toggle" }, { description: "Play sounds on agent state transitions." }),
  field("sound", "server_volume", "Server volume", { kind: "custom", id: "sound-volume" }),
  field("sound", "browser_volume", "Browser volume", { kind: "custom", id: "sound-volume" }),
  field("sound", "browser_scope", "Browser scope", {
    kind: "select",
    options: [
      { value: "all", label: "All sessions" },
      { value: "other_sessions", label: "Other sessions" },
      { value: "hidden_tab", label: "Hidden tab only" },
    ],
  }),
  field("sound", "browser_idle_enabled", "Browser: Idle", { kind: "toggle" }, { advanced: true }),
];

vi.mock("../../../lib/api", () => ({
  fetchSuggestions: vi.fn(() => Promise.resolve(null)),
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(() => Promise.resolve({ tmux: {}, logging: {}, session: {}, sound: {} })),
  getSettingsSchema: vi.fn(() => Promise.resolve(SCHEMA)),
  updateSettings: vi.fn(() => Promise.resolve("committed" as const)),
  updateTheme: vi.fn(() => Promise.resolve("committed")),
  fetchThemes: vi.fn(() => Promise.resolve([])),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

function renderTab(tab: string) {
  return render(<SettingsView onClose={() => {}} tab={tab} onSelectTab={() => {}} onServerAboutRefresh={() => {}} />);
}

function selectByLabel(container: HTMLElement, label: string): HTMLSelectElement {
  return within(container).getByRole("combobox", { name: label }) as HTMLSelectElement;
}

function numberInputByLabel(container: HTMLElement, label: string): HTMLInputElement {
  return within(container).getByRole("spinbutton", { name: label }) as HTMLInputElement;
}

// NumberField only accepts typing while focused and commits on blur.
function commit(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

function clickToggle(container: HTMLElement, label: string) {
  fireEvent.click(within(container).getByRole("switch", { name: label }));
}

import { resetSettingsStore } from "../../../lib/settingsStore";

beforeEach(resetSettingsStore);

describe("schema-driven settings field PATCH payloads", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(api.fetchSettings).mockResolvedValue({
      tmux: {},
      logging: {},
      session: { sidebar_position: "left" },
      sound: {},
    } as never);
  });

  it("saves Sidebar Position for the selected profile and reloads the saved value after a failed edit", async () => {
    const { container } = renderTab("session");
    await screen.findByText("Sidebar Position");
    const select = selectByLabel(container, "Sidebar Position");
    await waitFor(() => expect(select.value).toBe("left"));

    for (const value of ["right", "left"]) {
      fireEvent.change(select, { target: { value } });
      await waitFor(() =>
        expect(api.updateSettings).toHaveBeenLastCalledWith({ session: { sidebar_position: value } }, "main"),
      );
      expect(select.value).toBe(value);
    }

    vi.mocked(api.updateSettings).mockResolvedValueOnce("rejected");
    fireEvent.change(select, { target: { value: "right" } });
    await screen.findByText("Failed to save, please try again");
    await waitFor(() => expect(select.value).toBe("left"));
  });

  it("a tmux field edit never leaks sibling fields into the PATCH (sparse leaf)", async () => {
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

    await waitFor(() => expect(vi.mocked(api.updateSettings)).toHaveBeenCalled());
    expect(vi.mocked(api.updateSettings)).toHaveBeenCalledWith({ tmux: { status_bar: "disabled" } }, "main");
    // No call carries the untouched `mouse` field.
    for (const [updates] of vi.mocked(api.updateSettings).mock.calls) {
      expect((updates as { tmux?: Record<string, unknown> }).tmux).not.toHaveProperty("mouse");
    }
  });

  it("edits paired terminal sizing in the Terminal UI group with sparse numeric patches", async () => {
    const { container } = renderTab("session");
    await screen.findByText("Snooze Duration (minutes)");
    const fold = screen.getByRole("button", { name: /Terminal UI/ });
    expect(screen.queryByText("Paired terminal share of the split (%)")).toBeNull();
    fireEvent.click(fold);
    for (const [name, label, min, max, value] of [
      ["split_terminal_height_percent", "Paired terminal share of the split (%)", "1", "99", "30"],
      ["split_terminal_min_width", "Split only when at least this wide", "0", "1000", "110"],
      ["split_terminal_min_height", "Split only when at least this tall", "0", "1000", "35"],
    ]) {
      const input = numberInputByLabel(container, label);
      expect(input.min).toBe(min);
      expect(input.max).toBe(max);
      commit(input, value);
      await waitFor(() =>
        expect(api.updateSettings).toHaveBeenCalledWith({ session: { [name]: Number(value) } }, "main"),
      );
    }
  });

  it("session poller-thread ceiling is an advanced, global-only number that emits { session: { session_id_poller_max_threads } }", async () => {
    const LABEL = "Session-id poller threads";
    const { container } = renderTab("session");
    await screen.findByText("Snooze Duration (minutes)");

    expect(screen.queryByText(LABEL)).toBeNull();
    const fold = Array.from(container.querySelectorAll("button[aria-expanded]")).find((b) =>
      b.textContent?.includes("Advanced"),
    ) as HTMLButtonElement;
    expect(fold).toBeTruthy();
    expect(fold.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(fold);
    await screen.findByText(LABEL);

    const input = numberInputByLabel(container, LABEL);
    expect(input.min).toBe("0");
    expect(input.max).toBe("");

    commit(input, "120");

    await waitFor(() =>
      expect(vi.mocked(api.updateSettings)).toHaveBeenCalledWith(
        { session: { session_id_poller_max_threads: 120 } },
        "main",
      ),
    );
  });

  it("sound Enabled toggle emits { sound: { enabled: true } }", async () => {
    const { container } = renderTab("sound");
    await screen.findByText("Enabled");

    clickToggle(container, "Enabled");

    await waitFor(() =>
      expect(vi.mocked(api.updateSettings)).toHaveBeenCalledWith({ sound: { enabled: true } }, "main"),
    );
  });

  it("sound controls emit typed PATCH leaves for scope, an advanced event switch and both volumes", async () => {
    vi.mocked(api.fetchSettings).mockResolvedValue({
      tmux: {},
      logging: {},
      session: {},
      sound: { browser_idle_enabled: true },
    } as never);
    const { container } = renderTab("sound");
    await screen.findByText("Browser scope");

    fireEvent.change(selectByLabel(container, "Browser scope"), { target: { value: "hidden_tab" } });
    await waitFor(() =>
      expect(api.updateSettings).toHaveBeenLastCalledWith({ sound: { browser_scope: "hidden_tab" } }, "main"),
    );

    const fold = Array.from(container.querySelectorAll("button[aria-expanded]")).find((b) =>
      b.textContent?.includes("Advanced"),
    ) as HTMLButtonElement;
    fireEvent.click(fold);
    await screen.findByText("Browser: Idle");
    clickToggle(container, "Browser: Idle");
    await waitFor(() =>
      expect(api.updateSettings).toHaveBeenLastCalledWith({ sound: { browser_idle_enabled: false } }, "main"),
    );

    for (const [label, key, value] of [
      ["Browser volume", "browser_volume", 0.5],
      ["Server volume", "server_volume", 1.2],
    ] as const) {
      const range = within(container).getByRole("slider", { name: label });
      fireEvent.change(range, { target: { value: String(value) } });
      await waitFor(() => expect(api.updateSettings).toHaveBeenLastCalledWith({ sound: { [key]: value } }, "main"));
    }
  });
});
