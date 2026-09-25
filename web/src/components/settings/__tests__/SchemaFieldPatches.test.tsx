// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    "Session-id poller threads (restart req.)",
    { kind: "number", min: 0 },
    { profile_overridable: false, advanced: true, description: "Ceiling on concurrent session-id poller threads." },
  ),
  field("sound", "enabled", "Enabled", { kind: "toggle" }, { description: "Play sounds on agent state transitions." }),
];

vi.mock("../../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(() => Promise.resolve({ tmux: {}, logging: {}, session: {}, sound: {} })),
  getSettingsSchema: vi.fn(() => Promise.resolve(SCHEMA)),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  updateSettings: vi.fn(() => Promise.resolve(true)),
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

// FormFields labels are not wired to their controls, so walk from the label.
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

// NumberField only accepts typing while focused and commits on blur.
function commit(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

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
    vi.mocked(api.fetchSettings).mockResolvedValue({
      tmux: {},
      logging: {},
      session: { sidebar_position: "left" },
      sound: {},
    } as never);
  });

  it("saves Sidebar Position globally and reloads the saved value after a failed edit", async () => {
    const { container } = renderTab("session");
    await screen.findByText("Sidebar Position");
    const select = selectByLabel(container, "Sidebar Position");
    await waitFor(() => expect(select.value).toBe("left"));

    for (const value of ["right", "left"]) {
      fireEvent.change(select, { target: { value } });
      await waitFor(() =>
        expect(api.updateSettings).toHaveBeenLastCalledWith({ session: { sidebar_position: value } }),
      );
      expect(select.value).toBe(value);
    }
    expect(api.updateProfileSettings).not.toHaveBeenCalled();

    vi.mocked(api.updateSettings).mockResolvedValueOnce(false);
    fireEvent.change(select, { target: { value: "right" } });
    await screen.findByText("Failed to save, please try again");
    await waitFor(() => expect(select.value).toBe("left"));
  });

  it.each([
    ["tmux", "Status Bar", "disabled", { tmux: { status_bar: "disabled" } }],
    ["tmux", "Mouse Support", "disabled", { tmux: { mouse: "disabled" } }],
    ["logging", "Default level", "debug", { logging: { default_level: "debug" } }],
  ])("%s %s select emits its leaf", async (tab, label, value, patch) => {
    const { container } = renderTab(tab);
    await screen.findByText(label);
    fireEvent.change(selectByLabel(container, label), { target: { value } });
    if (tab === "logging") {
      await waitFor(() => expect(api.updateSettings).toHaveBeenCalledWith(patch));
      expect(api.updateProfileSettings).not.toHaveBeenCalled();
    } else {
      await waitFor(() => expect(api.updateProfileSettings).toHaveBeenCalledWith("main", patch));
      expect(api.updateSettings).not.toHaveBeenCalled();
    }
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

    await waitFor(() => expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalled());
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      tmux: { status_bar: "disabled" },
    });
    // No call carries the untouched `mouse` field.
    for (const [, updates] of vi.mocked(api.updateProfileSettings).mock.calls) {
      expect((updates as { tmux?: Record<string, unknown> }).tmux).not.toHaveProperty("mouse");
    }
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

  it("session poller-thread ceiling is an advanced, global-only number that emits { session: { session_id_poller_max_threads } }", async () => {
    const LABEL = "Session-id poller threads (restart req.)";
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

    const labelEl = Array.from(container.querySelectorAll("label")).find((l) => l.textContent === LABEL);
    expect(labelEl?.parentElement?.textContent).toContain("Applies to all profiles (not profile-overridable).");

    const input = numberInputByLabel(container, LABEL);
    expect(input.min).toBe("0");
    expect(input.max).toBe("");

    commit(input, "120");

    await waitFor(() =>
      expect(vi.mocked(api.updateSettings)).toHaveBeenCalledWith({
        session: { session_id_poller_max_threads: 120 },
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
