// @vitest-environment jsdom
//
// Contract test for the LoggingSettings panel. Covers the default level
// select, per-target override add/remove (with the delete-when-empty
// semantics), and every sink/rotation field. Part of #1217.

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { LoggingSettings } from "../LoggingSettings";

function mount(initial: Record<string, unknown> = {}) {
  const onSaveField = vi.fn();
  const onUpdate = vi.fn();
  const { container } = render(
    <LoggingSettings
      settings={{ logging: initial }}
      onSaveField={onSaveField}
      onUpdate={onUpdate}
    />,
  );
  return { onSaveField, onUpdate, container };
}

function commitText(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

function commitNumber(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

// The sink & rotation fields now live inside a default-collapsed "Advanced"
// CollapsibleSection, so they are absent from the DOM until the fold is
// expanded. Click the fold trigger (the only button carrying aria-expanded)
// before reaching for those fields.
function expandAdvanced(container: HTMLElement) {
  const trigger = container.querySelector(
    "button[aria-expanded]",
  ) as HTMLButtonElement;
  fireEvent.click(trigger);
}

function findSelectByLabel(container: HTMLElement, label: string): HTMLSelectElement {
  const labels = container.querySelectorAll("label");
  for (const l of labels) {
    if (l.textContent === label) {
      const select = l.parentElement?.querySelector("select");
      if (select) return select as HTMLSelectElement;
    }
  }
  throw new Error(`select with label ${label} not found`);
}

function findNumberInputByLabel(container: HTMLElement, label: string): HTMLInputElement {
  const labels = container.querySelectorAll("label");
  for (const l of labels) {
    if (l.textContent === label) {
      const input = l.parentElement?.querySelector('input[type="number"]');
      if (input) return input as HTMLInputElement;
    }
  }
  throw new Error(`number input with label ${label} not found`);
}

describe("LoggingSettings contract", () => {
  it("default level select emits logging.default_level", () => {
    const { onSaveField, onUpdate, container } = mount({
      default_level: "info",
    });
    const select = findSelectByLabel(container, "Default level");
    fireEvent.change(select, { target: { value: "debug" } });
    expect(onSaveField).toHaveBeenCalledWith(
      "logging",
      "default_level",
      "debug",
    );
    expect(onUpdate).toHaveBeenCalledWith({
      logging: expect.objectContaining({ default_level: "debug" }),
    });
  });

  it("per-target select adds a new key to logging.targets", () => {
    const { onSaveField, container } = mount({ default_level: "info" });
    const select = findSelectByLabel(container, "acp.protocol");
    fireEvent.change(select, { target: { value: "trace" } });
    expect(onSaveField).toHaveBeenCalledWith("logging", "targets", {
      "acp.protocol": "trace",
    });
  });

  it("per-target select preserves existing overrides when adding", () => {
    const { onSaveField, container } = mount({
      default_level: "info",
      targets: { "auth.token": "warn" },
    });
    const select = findSelectByLabel(container, "acp.protocol");
    fireEvent.change(select, { target: { value: "debug" } });
    expect(onSaveField).toHaveBeenCalledWith("logging", "targets", {
      "auth.token": "warn",
      "acp.protocol": "debug",
    });
  });

  it("per-target select with empty value deletes the override", () => {
    const { onSaveField, container } = mount({
      default_level: "info",
      targets: { "acp.protocol": "trace", "auth.token": "warn" },
    });
    const select = findSelectByLabel(container, "acp.protocol");
    fireEvent.change(select, { target: { value: "" } });
    expect(onSaveField).toHaveBeenCalledWith("logging", "targets", {
      "auth.token": "warn",
    });
  });

  it("sink & rotation fields are hidden until the Advanced fold is expanded", () => {
    const { container } = mount({});
    expect(() => findSelectByLabel(container, "Output")).toThrow();
    expandAdvanced(container);
    expect(findSelectByLabel(container, "Output")).toBeTruthy();
  });

  it("output select emits logging.output", () => {
    const { onSaveField, container } = mount({});
    expandAdvanced(container);
    const select = findSelectByLabel(container, "Output");
    fireEvent.change(select, { target: { value: "stdout" } });
    expect(onSaveField).toHaveBeenCalledWith("logging", "output", "stdout");
  });

  it("file_path text commits on blur", () => {
    const { onSaveField, container } = mount({ file_path: "debug.log" });
    expandAdvanced(container);
    // The TextField wraps an <input type=text>; the first such input in the
    // panel is the file_path field.
    const input = container.querySelector(
      "input[type=text]",
    ) as HTMLInputElement;
    commitText(input, "/var/log/aoe.log");
    expect(onSaveField).toHaveBeenCalledWith(
      "logging",
      "file_path",
      "/var/log/aoe.log",
    );
  });

  it("file_path falls back to 'debug.log' when blanked", () => {
    const { onSaveField, container } = mount({ file_path: "custom.log" });
    expandAdvanced(container);
    const input = container.querySelector(
      "input[type=text]",
    ) as HTMLInputElement;
    commitText(input, "   ");
    expect(onSaveField).toHaveBeenCalledWith(
      "logging",
      "file_path",
      "debug.log",
    );
  });

  it("rotation select emits logging.rotation", () => {
    const { onSaveField, container } = mount({});
    expandAdvanced(container);
    const select = findSelectByLabel(container, "Rotation");
    fireEvent.change(select, { target: { value: "never" } });
    expect(onSaveField).toHaveBeenCalledWith("logging", "rotation", "never");
  });

  it("max_size_mib commits a numeric value", () => {
    const { onSaveField, container } = mount({ max_size_mib: 50 });
    expandAdvanced(container);
    const input = findNumberInputByLabel(container, "Max size (MiB)");
    commitNumber(input, "256");
    expect(onSaveField).toHaveBeenCalledWith("logging", "max_size_mib", 256);
  });

  it("keep_count commits a numeric value", () => {
    const { onSaveField, container } = mount({ keep_count: 5 });
    expandAdvanced(container);
    const input = findNumberInputByLabel(container, "Keep count");
    commitNumber(input, "10");
    expect(onSaveField).toHaveBeenCalledWith("logging", "keep_count", 10);
  });

  it("show_spans toggle emits logging.show_spans", () => {
    const { onSaveField, container } = mount({ show_spans: false });
    expandAdvanced(container);
    const toggle = container.querySelector(
      "button[role=switch]",
    ) as HTMLButtonElement;
    fireEvent.click(toggle);
    expect(onSaveField).toHaveBeenCalledWith("logging", "show_spans", true);
  });
});
