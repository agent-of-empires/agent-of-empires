// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SchemaSection } from "../SchemaSection";
import type { SettingsFieldDescriptor } from "../../../lib/types";

function descriptor(
  over: Partial<SettingsFieldDescriptor> & Pick<SettingsFieldDescriptor, "field" | "label" | "widget">,
): SettingsFieldDescriptor {
  return {
    section: "sandbox",
    category: "Sandbox",
    description: "",
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
    ...over,
  };
}

const SCHEMA: SettingsFieldDescriptor[] = [
  descriptor({ field: "enabled_by_default", label: "Sandbox enabled by default", widget: { kind: "toggle" } }),
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
  descriptor({
    field: "node_path",
    label: "Node path",
    widget: { kind: "text" },
    web_write: { policy: "local_only", reason: "host binary" },
  }),
  descriptor({ section: "worktree", field: "enabled", label: "Worktrees enabled", widget: { kind: "toggle" } }),
];

function mount(schema: SettingsFieldDescriptor[], values: Record<string, unknown> = {}, extra = {}) {
  const onSaveField = vi.fn(() => true);
  const { container } = render(
    <SchemaSection section={schema[0]!.section} schema={schema} values={values} onSaveField={onSaveField} {...extra} />,
  );
  return { onSaveField, container };
}

describe("SchemaSection", () => {
  it("renders this section's web-writable fields, folds advanced ones, and emits (section, field, value)", () => {
    const { onSaveField, container } = mount(SCHEMA, { enabled_by_default: false, default_terminal_mode: "host" });
    expect(screen.queryByText("Node path")).toBeNull();
    expect(screen.queryByText("Worktrees enabled")).toBeNull();
    fireEvent.click(container.querySelector("button[role=switch]")!);
    expect(onSaveField).toHaveBeenCalledWith("sandbox", "enabled_by_default", true);
    fireEvent.change(container.querySelector("select")!, { target: { value: "container" } });
    expect(onSaveField).toHaveBeenCalledWith("sandbox", "default_terminal_mode", "container");
    expect(screen.queryByText("Extra volumes")).toBeNull();
    fireEvent.click(screen.getByText("Advanced"));
    expect(screen.getByText("Extra volumes")).toBeTruthy();
  });

  it("renders a registered custom widget and a visible fallback for an unknown one", () => {
    const { container } = mount(
      [
        descriptor({
          section: "sound",
          field: "volume",
          label: "Volume",
          widget: { kind: "custom", id: "sound-volume" },
        }),
        descriptor({
          section: "sound",
          field: "mystery",
          label: "Mystery",
          widget: { kind: "custom", id: "no-such-widget" },
        }),
      ],
      { volume: 1.0 },
    );
    expect(container.querySelector<HTMLInputElement>('input[type="range"]')?.max).toBe("1.5");
    expect(screen.getByText(/No web control registered/).textContent).toContain("no-such-widget");
  });

  it("runs onAfterSave after a successful save", async () => {
    const onAfterSave = vi.fn();
    const { container } = mount(
      [descriptor({ section: "acp", field: "show_tool_durations", label: "Durations", widget: { kind: "toggle" } })],
      { show_tool_durations: false },
      { onAfterSave },
    );
    fireEvent.click(container.querySelector("button[role=switch]")!);
    await waitFor(() =>
      expect(onAfterSave).toHaveBeenCalledWith(expect.objectContaining({ field: "show_tool_durations" }), true),
    );
  });

  it("validates env_list entries before saving", () => {
    const { onSaveField } = mount(
      [descriptor({ field: "environment", label: "Env", widget: { kind: "list" }, validation: { rule: "env_list" } })],
      { environment: [] },
    );
    fireEvent.click(screen.getByText("+ Add"));
    const input = screen.getByRole("textbox");
    for (const value of ["1bad", "FOO=bar"]) {
      fireEvent.change(input, { target: { value } });
      fireEvent.keyDown(input, { key: "Enter" });
    }
    expect(onSaveField).toHaveBeenCalledTimes(1);
    expect(onSaveField).toHaveBeenCalledWith("sandbox", "environment", ["FOO=bar"]);
  });
});
