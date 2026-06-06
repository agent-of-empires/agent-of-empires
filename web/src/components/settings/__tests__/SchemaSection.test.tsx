// @vitest-environment jsdom
//
// Contract test for the generic schema-driven settings renderer (#1692).
// SchemaSection turns `GET /api/settings/schema` descriptors into FormFields
// rows; this pins that (a) it renders the right control per widget, (b) edits
// emit onSaveField with the exact (section, field, value) shape, (c) advanced
// fields are tucked under a fold, and (d) `local_only` fields are never shown
// (the server rejects writes to them).

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SchemaSection } from "../SchemaSection";
import type { SettingsFieldDescriptor } from "../../../lib/types";

const ALLOW = { policy: "allow" } as const;
const NONE = { rule: "none" } as const;

function descriptor(
  over: Partial<SettingsFieldDescriptor> &
    Pick<SettingsFieldDescriptor, "field" | "label" | "widget">,
): SettingsFieldDescriptor {
  return {
    section: "sandbox",
    category: "Sandbox",
    description: "",
    web_write: ALLOW,
    profile_overridable: true,
    validation: NONE,
    advanced: false,
    ...over,
  };
}

const SCHEMA: SettingsFieldDescriptor[] = [
  descriptor({
    field: "enabled_by_default",
    label: "Sandbox enabled by default",
    widget: { kind: "toggle" },
  }),
  descriptor({
    field: "default_terminal_mode",
    label: "Default terminal mode",
    widget: {
      kind: "select",
      options: [
        { value: "host", label: "Host" },
        { value: "container", label: "Container" },
      ],
    },
  }),
  descriptor({
    field: "extra_volumes",
    label: "Extra volumes",
    widget: { kind: "list" },
    validation: { rule: "volume_list" },
    advanced: true,
  }),
  // local_only: a host execution surface the server rejects; must not render.
  descriptor({
    field: "node_path",
    label: "Node path",
    widget: { kind: "text" },
    web_write: { policy: "local_only", reason: "host binary" },
  }),
  // A field from another section must be ignored by this SchemaSection.
  descriptor({
    section: "worktree",
    field: "enabled",
    label: "Worktrees enabled",
    widget: { kind: "toggle" },
  }),
];

function mount(values: Record<string, unknown> = {}) {
  const onSaveField = vi.fn();
  const { container } = render(
    <SchemaSection
      section="sandbox"
      schema={SCHEMA}
      values={values}
      onSaveField={onSaveField}
    />,
  );
  return { onSaveField, container };
}

describe("SchemaSection contract", () => {
  it("renders only this section's web-writable fields", () => {
    mount({ enabled_by_default: false, default_terminal_mode: "host" });
    expect(screen.getByText("Sandbox enabled by default")).toBeTruthy();
    expect(screen.getByText("Default terminal mode")).toBeTruthy();
    // local_only and other-section fields are skipped.
    expect(screen.queryByText("Node path")).toBeNull();
    expect(screen.queryByText("Worktrees enabled")).toBeNull();
  });

  it("toggle emits (section, field, value)", () => {
    const { onSaveField, container } = mount({ enabled_by_default: false });
    const toggle = container.querySelector(
      "button[role=switch]",
    ) as HTMLButtonElement;
    fireEvent.click(toggle);
    expect(onSaveField).toHaveBeenCalledWith(
      "sandbox",
      "enabled_by_default",
      true,
    );
  });

  it("select emits the chosen option value", () => {
    const { onSaveField, container } = mount({
      default_terminal_mode: "host",
    });
    const select = container.querySelector("select") as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "container" } });
    expect(onSaveField).toHaveBeenCalledWith(
      "sandbox",
      "default_terminal_mode",
      "container",
    );
  });

  it("advanced fields live under an Advanced fold", () => {
    mount({});
    // The advanced "Extra volumes" field is gated behind the fold toggle.
    expect(screen.getByText("Advanced")).toBeTruthy();
    expect(screen.queryByText("Extra volumes")).toBeNull();
    fireEvent.click(screen.getByText("Advanced"));
    expect(screen.getByText("Extra volumes")).toBeTruthy();
  });
});

describe("SchemaSection custom widgets, hooks, and validators (#1792)", () => {
  it("renders a registered custom widget via its widget.id", () => {
    render(
      <SchemaSection
        section="sound"
        schema={[
          descriptor({
            section: "sound",
            field: "mode",
            label: "Mode",
            widget: { kind: "custom", id: "sound-mode" },
          }),
        ]}
        values={{ mode: "random" }}
        onSaveField={vi.fn()}
      />,
    );
    // SoundModeWidget renders a select with the Random/Specific options.
    const select = screen
      .getByText("Mode")
      .parentElement?.querySelector("select") as HTMLSelectElement;
    expect(select).toBeTruthy();
    expect(select.value).toBe("random");
  });

  it("shows a visible fallback for an unregistered custom widget", () => {
    render(
      <SchemaSection
        section="sound"
        schema={[
          descriptor({
            section: "sound",
            field: "mystery",
            label: "Mystery knob",
            widget: { kind: "custom", id: "no-such-widget" },
          }),
        ]}
        values={{}}
        onSaveField={vi.fn()}
      />,
    );
    expect(screen.getByText(/No web control registered/)).toBeTruthy();
    expect(screen.getByText(/no-such-widget/)).toBeTruthy();
  });

  it("runs onAfterSave once after a successful save", async () => {
    const onSaveField = vi.fn(() => true);
    const onAfterSave = vi.fn();
    const { container } = render(
      <SchemaSection
        section="acp"
        schema={[
          descriptor({
            section: "acp",
            field: "show_tool_durations",
            label: "Show tool-call durations",
            widget: { kind: "toggle" },
          }),
        ]}
        values={{ show_tool_durations: false }}
        onSaveField={onSaveField}
        onAfterSave={onAfterSave}
      />,
    );
    fireEvent.click(container.querySelector("button[role=switch]")!);
    await waitFor(() =>
      expect(onAfterSave).toHaveBeenCalledWith(
        expect.objectContaining({
          section: "acp",
          field: "show_tool_durations",
        }),
        true,
      ),
    );
  });

  it("flags non-overridable (global-only) fields in their description", () => {
    render(
      <SchemaSection
        section="web"
        schema={[
          descriptor({
            section: "web",
            field: "notify_on_idle",
            label: "Notify on idle",
            widget: { kind: "toggle" },
            profile_overridable: false,
          }),
        ]}
        values={{}}
        onSaveField={vi.fn()}
      />,
    );
    expect(screen.getByText(/Applies to all profiles/)).toBeTruthy();
  });

  it("rejects a malformed env entry and accepts a valid one (env_list)", () => {
    const onSaveField = vi.fn();
    render(
      <SchemaSection
        section="sandbox"
        schema={[
          descriptor({
            field: "environment",
            label: "Environment variables",
            widget: { kind: "list" },
            validation: { rule: "env_list" },
          }),
        ]}
        values={{ environment: [] }}
        onSaveField={onSaveField}
      />,
    );
    const root = screen.getByText("Environment variables").parentElement
      ?.parentElement as HTMLElement;
    const addBtn = screen
      .getByText("Environment variables")
      .parentElement?.querySelector("button") as HTMLButtonElement;
    fireEvent.click(addBtn);
    const input = root.querySelector('input[type="text"]') as HTMLInputElement;

    // Invalid: leading digit. The validator blocks the commit.
    fireEvent.change(input, { target: { value: "1bad" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSaveField).not.toHaveBeenCalled();

    // Valid KEY=VALUE commits.
    fireEvent.change(input, { target: { value: "FOO=bar" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSaveField).toHaveBeenCalledWith("sandbox", "environment", [
      "FOO=bar",
    ]);
  });
});
