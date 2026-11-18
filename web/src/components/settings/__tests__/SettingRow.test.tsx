// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import { writeClipboard } from "../../../lib/clipboard";
import type { SettingsFieldDescriptor, SettingsWidget } from "../../../lib/types";
import { SchemaSection } from "../SchemaSection";
import { descriptor } from "./fixtures";

vi.mock("../../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));
vi.mock("../../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../../lib/api")>()),
  resolvePluginOptions: vi.fn().mockResolvedValue([]),
}));

function mount(schema: SettingsFieldDescriptor[], values: Record<string, unknown> = {}) {
  const onSaveField = vi.fn(() => Promise.resolve(true));
  const section = schema[0]!.section;
  const utils = render(<SchemaSection section={section} schema={schema} values={values} onSaveField={onSaveField} />);
  return { ...utils, onSaveField };
}

const rowOf = (label: string) =>
  screen.getByText(label, { selector: "[id$='-label']" }).closest("[data-settings-field]") as HTMLElement;

describe("SettingRow", () => {
  it.each<[string, SettingsWidget, string]>([
    ["toggle", { kind: "toggle" }, "switch"],
    ["text", { kind: "text" }, "textbox"],
    ["multiline text", { kind: "text", multiline: true }, "textbox"],
    ["optional text", { kind: "optional_text" }, "textbox"],
    ["cron", { kind: "cron" }, "textbox"],
    ["number", { kind: "number", min: 0 }, "spinbutton"],
    ["slider", { kind: "slider", min: 0, max: 10, step: 1 }, "slider"],
    ["custom slider", { kind: "custom", id: "sound-volume" }, "slider"],
    ["select", { kind: "select", options: [{ value: "a", label: "A" }] }, "combobox"],
    ["dynamic select", { kind: "dynamic_select", source: "projects" }, "combobox"],
    ["list", { kind: "list" }, "group"],
    ["object list", { kind: "object_list", id_field: "id", fields: [] }, "group"],
  ])("names a %s control after the row label", (_, widget, role) => {
    mount([descriptor({ section: "x", field: "f", label: "My setting", widget })]);
    expect(screen.getByRole(role, { name: "My setting" })).toBeTruthy();
  });

  it("shows the summary as the plain line, never the developer description or profile note", () => {
    mount([
      descriptor({
        section: "x",
        field: "with_summary",
        label: "Has summary",
        summary: "What it does for you.",
        description: "Developer detail, see #1234.",
        profile_overridable: false,
      }),
      descriptor({ section: "x", field: "bare", label: "No summary", description: "Developer detail only." }),
    ]);
    mount([
      descriptor({ section: "plugin:demo", field: "p", label: "Plugin knob", description: "Developer detail too." }),
    ]);
    expect(within(rowOf("Has summary")).getByText("What it does for you.")).toBeTruthy();
    expect(screen.queryByText(/Developer detail/)).toBeNull();
    expect(screen.queryByText(/Applies to all profiles/)).toBeNull();
    expect(rowOf("No summary").textContent).toBe("No summaryx.bare");
    // Plugin manifests carry no summary, so plugin rows show their label and key only, like core rows.
    expect(rowOf("Plugin knob").textContent).toBe("Plugin knobplugin:demo.p");
  });

  it("puts the tags after the label and a lock only on passphrase rows", () => {
    mount([
      descriptor({
        section: "x",
        field: "locked",
        label: "Locked",
        restart: true,
        web_write: { policy: "requires_elevation", reason: "r" },
      }),
      descriptor({ section: "x", field: "open", label: "Open", web_only: true }),
    ]);
    const locked = within(rowOf("Locked"));
    expect(locked.getByText("Needs restart")).toBeTruthy();
    expect(locked.getByRole("img", { name: "Asks for the passphrase" })).toBeTruthy();
    const open = within(rowOf("Open"));
    expect(open.getByText("Web dashboard only")).toBeTruthy();
    expect(open.queryByRole("img", { name: "Asks for the passphrase" })).toBeNull();
    // Tags never leak into the control's accessible name.
    expect(screen.getByRole("switch", { name: "Locked" })).toBeTruthy();
  });

  it("copies the section.field key and shows it was copied, without flipping a toggle", async () => {
    const { onSaveField } = mount([descriptor({ section: "logging", field: "show_spans", label: "Show spans" })]);
    const row = within(rowOf("Show spans"));
    expect(row.getByText("logging.show_spans")).toBeTruthy();

    await act(async () => fireEvent.click(row.getByRole("button", { name: "Copy config key" })));

    expect(writeClipboard).toHaveBeenCalledWith("logging.show_spans");
    expect(row.getByText("Copied")).toBeTruthy();
    expect(onSaveField).not.toHaveBeenCalled();
  });

  it("shows no copied state when the clipboard refuses", async () => {
    vi.mocked(writeClipboard).mockResolvedValueOnce(false);
    mount([descriptor({ section: "acp", field: "replay_events", label: "Keep the last" })]);
    const row = within(rowOf("Keep the last"));

    await act(async () => fireEvent.click(row.getByRole("button", { name: "Copy config key" })));

    expect(writeClipboard).toHaveBeenCalledWith("acp.replay_events");
    expect(row.queryByText("Copied")).toBeNull();
  });

  it("toggles a switch from its label and plain line", async () => {
    const { onSaveField } = mount(
      [descriptor({ section: "x", field: "flag", label: "Flag", summary: "Turns the thing on." })],
      { flag: false },
    );
    fireEvent.click(screen.getByText("Turns the thing on."));
    await waitFor(() => expect(onSaveField).toHaveBeenCalledWith("x", "flag", true));
  });
});
