// @vitest-environment jsdom
//
// Contract test for plugin settings rendering (#2094). PluginSettingsSections
// turns the virtual `plugin:<id>` schema sections into the generic
// SchemaSection rows, seeds the manifest default until a value is stored, and
// saves through the global PATCH (`updateSettings`) with the `plugin:<id>`
// section the server folds into `plugins.<id>.settings`.

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("../../../lib/api", () => ({
  updateSettings: vi.fn().mockResolvedValue(true),
}));

import { updateSettings } from "../../../lib/api";
import { PluginSettingsSections } from "../PluginSettingsSections";
import type { SettingsFieldDescriptor } from "../../../lib/types";

const ALLOW = { policy: "allow" } as const;
const NONE = { rule: "none" } as const;

const SCHEMA: SettingsFieldDescriptor[] = [
  // A core section is ignored by this component.
  {
    section: "theme",
    field: "idle_decay_minutes",
    category: "Theme",
    label: "Idle Decay",
    description: "",
    widget: { kind: "number" },
    web_write: ALLOW,
    profile_overridable: true,
    validation: NONE,
    advanced: false,
  },
  {
    section: "plugin:acme.kit",
    field: "enabled",
    category: "Plugins",
    label: "Enabled",
    description: "",
    widget: { kind: "toggle" },
    web_write: ALLOW,
    profile_overridable: false,
    validation: NONE,
    advanced: false,
    default: true,
  },
  {
    section: "plugin:acme.kit",
    field: "retries",
    category: "Plugins",
    label: "Retries",
    description: "",
    widget: { kind: "number" },
    web_write: ALLOW,
    profile_overridable: false,
    validation: { rule: "range_u64", min: 0, max: 5 },
    advanced: false,
    default: 3,
  },
];

describe("PluginSettingsSections", () => {
  it("renders nothing when no plugin sections exist", () => {
    const { container } = render(
      <PluginSettingsSections schema={SCHEMA.slice(0, 1)} settings={{}} onSaved={() => {}} />,
    );
    expect(container.textContent).toBe("");
  });

  it("renders plugin fields, seeding the manifest default when unstored", () => {
    render(<PluginSettingsSections schema={SCHEMA} settings={{ plugins: {} }} onSaved={() => {}} />);
    expect(screen.getByText("acme.kit")).toBeTruthy();
    expect(screen.getByText("Retries")).toBeTruthy();
    // The number field shows the seeded default of 3.
    const retries = screen.getByDisplayValue("3");
    expect(retries).toBeTruthy();
  });

  it("prefers the stored value over the default", () => {
    render(
      <PluginSettingsSections
        schema={SCHEMA}
        settings={{ plugins: { "acme.kit": { settings: { retries: 4 } } } }}
        onSaved={() => {}}
      />,
    );
    expect(screen.getByDisplayValue("4")).toBeTruthy();
  });

  it("saves through the global PATCH with the plugin:<id> section", async () => {
    const onSaved = vi.fn();
    render(<PluginSettingsSections schema={SCHEMA} settings={{ plugins: {} }} onSaved={onSaved} />);
    fireEvent.click(screen.getByRole("switch"));
    await waitFor(() => {
      expect(updateSettings).toHaveBeenCalledWith({ "plugin:acme.kit": { enabled: false } });
    });
    expect(onSaved).toHaveBeenCalled();
  });
});
