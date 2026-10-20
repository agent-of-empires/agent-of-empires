// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi, beforeEach } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { fetchSettings } from "../../lib/api";
import { SettingsView } from "../SettingsView";

const PROFILES = [{ name: "main", is_default: true }];

const THEME_SCHEMA = [
  {
    section: "theme",
    field: "name",
    category: "Theme",
    label: "Theme",
    description: "",
    widget: { kind: "custom", id: "theme-name" },
    web_write: { policy: "allow" },
    profile_overridable: false,
    validation: { rule: "none" },
    advanced: false,
  },
  {
    section: "theme",
    field: "color_mode",
    category: "Theme",
    label: "Color Mode",
    description: "",
    widget: {
      kind: "select",
      options: [
        { value: "truecolor", label: "truecolor" },
        { value: "palette", label: "palette" },
      ],
    },
    web_write: { policy: "allow" },
    profile_overridable: false,
    validation: { rule: "none" },
    advanced: false,
  },
  {
    section: "theme",
    field: "idle_decay_minutes",
    category: "Theme",
    label: "Idle Decay (minutes)",
    description: "",
    widget: { kind: "number", min: 0 },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  },
];

const updateTheme = vi.fn((_patch: Record<string, unknown>) =>
  Promise.resolve("committed" as "committed" | "rejected" | "uncertain"),
);
const updateSettings = vi.fn(() => Promise.resolve("committed" as const));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(() => Promise.resolve({ theme: { name: "empire", idle_decay_minutes: 0 } })),
  getSettingsSchema: vi.fn(() => Promise.resolve(THEME_SCHEMA)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
  updateSettings: (updates: Record<string, unknown>, profile?: string) => updateSettings(updates, profile),
  updateTheme: (patch: Record<string, unknown>) => updateTheme(patch),
  fetchThemes: vi.fn(() => Promise.resolve(["empire", "dracula"])),
}));

const dispatchThemePickerChanged = vi.fn();
vi.mock("../../hooks/useResolvedTheme", () => ({
  dispatchThemePickerChanged: (name?: string) => dispatchThemePickerChanged(name),
}));

afterEach(() => {
  cleanup();
  updateTheme.mockClear();
  updateSettings.mockClear();
  dispatchThemePickerChanged.mockClear();
});

function renderThemeTab() {
  return render(<SettingsView onClose={() => {}} tab="theme" onSelectTab={vi.fn()} onServerAboutRefresh={() => {}} />);
}

/** A <select> that carries an <option> with this value. */
function selectWithOption(value: string): HTMLSelectElement {
  const found = Array.from(document.querySelectorAll<HTMLSelectElement>("select")).find((s) =>
    Array.from(s.options).some((o) => o.value === value),
  );
  if (!found) throw new Error(`no <select> has an option "${value}"`);
  return found;
}

/** The <input> rendered next to a unique field label. */
function inputByLabel(text: string): HTMLInputElement {
  const input = screen.getByText(text).closest("div")?.querySelector("input");
  if (!input) throw new Error(`no <input> under label "${text}"`);
  return input;
}

import { resetSettingsStore } from "../../lib/settingsStore";

beforeEach(resetSettingsStore);

describe("SettingsView theme tab save routing", () => {
  it("writes the theme name to /api/theme, not the profile", async () => {
    renderThemeTab();
    // The theme dropdown is populated asynchronously from fetchThemes.
    await waitFor(() => selectWithOption("dracula"));
    fireEvent.change(selectWithOption("dracula"), {
      target: { value: "dracula" },
    });
    await waitFor(() => expect(updateTheme).toHaveBeenCalledWith({ name: "dracula" }));
    expect(updateSettings).not.toHaveBeenCalled();
  });

  // Ported from live settings-theme-color-mode.spec.ts.
  it("color-mode change PATCHes but never dispatches the theme repaint event", async () => {
    renderThemeTab();
    await waitFor(() => selectWithOption("palette"));
    fireEvent.change(selectWithOption("palette"), {
      target: { value: "palette" },
    });
    await waitFor(() => expect(updateTheme).toHaveBeenCalledWith({ color_mode: "palette" }));
    expect(dispatchThemePickerChanged).not.toHaveBeenCalled();
    expect(updateSettings).not.toHaveBeenCalled();

    // Positive control: a theme-name pick through the same tab does dispatch,
    // proving the spy is wired and the gating is per-field, not global.
    fireEvent.change(selectWithOption("dracula"), {
      target: { value: "dracula" },
    });
    await waitFor(() => expect(updateTheme).toHaveBeenCalledWith({ name: "dracula" }));
    await waitFor(() => expect(dispatchThemePickerChanged).toHaveBeenCalledWith("dracula"));
    expect(dispatchThemePickerChanged).toHaveBeenCalledTimes(1);
  });

  it("routes a profile-overridable row (idle decay) through the settings save, not /api/theme", async () => {
    renderThemeTab();
    await screen.findByText("Idle Decay (minutes)");
    const idle = inputByLabel("Idle Decay (minutes)");
    // NumberField re-syncs from its prop unless focused, so focus before typing.
    fireEvent.focus(idle);
    fireEvent.change(idle, { target: { value: "5" } });
    fireEvent.blur(idle);
    await waitFor(() => expect(updateSettings).toHaveBeenCalledWith({ theme: { idle_decay_minutes: 5 } }, "main"));
    expect(updateTheme).not.toHaveBeenCalled();
  });

  it("keeps a theme pick that stays unconfirmed on screen after its resends, without repainting or reporting a rejection", async () => {
    for (let i = 0; i < 3; i++) updateTheme.mockImplementationOnce(() => Promise.resolve("uncertain"));
    renderThemeTab();
    await waitFor(() => selectWithOption("dracula"));
    fireEvent.change(selectWithOption("dracula"), { target: { value: "dracula" } });

    await waitFor(() => expect(updateTheme).toHaveBeenCalledTimes(3));
    expect(updateTheme).toHaveBeenLastCalledWith({ name: "dracula" });
    expect(screen.queryByText("Failed to save, please try again")).toBeNull();
    expect(dispatchThemePickerChanged).not.toHaveBeenCalled();
  });

  it("repaints an uncertain theme pick once a fetch shows it saved", async () => {
    updateTheme.mockImplementationOnce(() => Promise.resolve("uncertain"));
    renderThemeTab();
    await waitFor(() => selectWithOption("dracula"));
    const saved = { theme: { name: "dracula", idle_decay_minutes: 0 } };
    vi.mocked(fetchSettings).mockResolvedValueOnce(saved).mockResolvedValueOnce(saved);
    fireEvent.change(selectWithOption("dracula"), { target: { value: "dracula" } });

    await waitFor(() => expect(dispatchThemePickerChanged).toHaveBeenCalledWith("dracula"));
    expect(updateTheme).toHaveBeenCalledTimes(1);
  });

  it("reports a rejected theme pick and never repaints", async () => {
    updateTheme.mockImplementationOnce(() => Promise.resolve("rejected"));
    renderThemeTab();
    await waitFor(() => selectWithOption("dracula"));
    fireEvent.change(selectWithOption("dracula"), { target: { value: "dracula" } });

    await screen.findByText("Failed to save, please try again");
    expect(dispatchThemePickerChanged).not.toHaveBeenCalled();
  });

  it("repaints once, from the retry, when an old first fetch leaves an uncertain pick unconfirmed", async () => {
    renderThemeTab();
    await waitFor(() => selectWithOption("dracula"));
    updateTheme.mockImplementationOnce(() => Promise.resolve("uncertain"));
    // The retry commits after the first confirmation GET already showed the old theme.
    updateTheme.mockImplementationOnce(() => Promise.resolve("committed"));
    fireEvent.change(selectWithOption("dracula"), { target: { value: "dracula" } });

    await waitFor(() => expect(dispatchThemePickerChanged).toHaveBeenCalledWith("dracula"));
    expect(updateTheme).toHaveBeenCalledTimes(2);
    expect(dispatchThemePickerChanged).toHaveBeenCalledTimes(1);
  });
});
