// @vitest-environment jsdom
//
// The theme is a global preference: the Settings theme tab must route the
// global-only fields (theme name, color mode) to the dedicated PATCH /api/theme
// endpoint, while a profile-overridable row in the same tab (idle decay) still
// writes the selected profile. Pins `saveThemeField`'s per-field routing so a
// regression can't quietly send the theme back into a profile (the
// empire->rose-pine flip). The end-to-end persist path lives in
// web/tests/live/settings-persistence-theme.spec.ts.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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

const updateTheme = vi.fn(() => Promise.resolve(true));
const updateProfileSettings = vi.fn(() => Promise.resolve(true));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() => Promise.resolve({ theme: { name: "empire", idle_decay_minutes: 0 } })),
  getSettingsSchema: vi.fn(() => Promise.resolve(THEME_SCHEMA)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
  updateProfileSettings: (name: string, updates: Record<string, unknown>) => updateProfileSettings(name, updates),
  updateTheme: (patch: Record<string, unknown>) => updateTheme(patch),
  fetchThemes: vi.fn(() => Promise.resolve(["empire", "dracula"])),
}));

vi.mock("../../hooks/useResolvedTheme", () => ({
  dispatchThemePickerChanged: vi.fn(),
}));

afterEach(() => {
  cleanup();
  updateTheme.mockClear();
  updateProfileSettings.mockClear();
});

function renderThemeTab() {
  return render(<SettingsView onClose={() => {}} tab="theme" onSelectTab={vi.fn()} onServerAboutRefresh={() => {}} />);
}

/** A <select> that carries an <option> with this value. Labels in FormFields
 *  are not wired to their controls, so we locate by option value rather than
 *  accessible name (and dodge the duplicated mobile/desktop tab strips). */
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

describe("SettingsView theme tab save routing", () => {
  it("writes the theme name to /api/theme, not the profile", async () => {
    renderThemeTab();
    // The theme dropdown is populated asynchronously from fetchThemes.
    await waitFor(() => selectWithOption("dracula"));
    fireEvent.change(selectWithOption("dracula"), {
      target: { value: "dracula" },
    });
    await waitFor(() => expect(updateTheme).toHaveBeenCalledWith({ name: "dracula" }));
    expect(updateProfileSettings).not.toHaveBeenCalled();
  });

  it("writes color mode to /api/theme too", async () => {
    renderThemeTab();
    await waitFor(() => selectWithOption("palette"));
    fireEvent.change(selectWithOption("palette"), {
      target: { value: "palette" },
    });
    await waitFor(() => expect(updateTheme).toHaveBeenCalledWith({ color_mode: "palette" }));
    expect(updateProfileSettings).not.toHaveBeenCalled();
  });

  it("routes a profile-overridable row (idle decay) to the profile, not /api/theme", async () => {
    renderThemeTab();
    await screen.findByText("Idle Decay (minutes)");
    const idle = inputByLabel("Idle Decay (minutes)");
    // NumberField re-syncs from its prop unless focused, so focus before typing.
    fireEvent.focus(idle);
    fireEvent.change(idle, { target: { value: "5" } });
    fireEvent.blur(idle);
    await waitFor(() =>
      expect(updateProfileSettings).toHaveBeenCalledWith("main", {
        theme: { idle_decay_minutes: 5 },
      }),
    );
    expect(updateTheme).not.toHaveBeenCalled();
  });
});
