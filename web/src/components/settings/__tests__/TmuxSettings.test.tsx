// @vitest-environment jsdom
//
// Contract test for the TmuxSettings panel. Two SelectFields for
// tmux.status_bar and tmux.mouse. See SoundSettings.test.tsx for the
// canonical pattern. Part of #1217.

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { TmuxSettings } from "../TmuxSettings";

function mount(initial: Record<string, unknown> = {}) {
  const onSaveField = vi.fn();
  const onUpdate = vi.fn();
  const { container } = render(
    <TmuxSettings
      settings={{ tmux: initial }}
      onSaveField={onSaveField}
      onUpdate={onUpdate}
    />,
  );
  return { onSaveField, onUpdate, container };
}

describe("TmuxSettings contract", () => {
  it("explains that font size is controlled outside tmux", () => {
    const { container } = mount({});

    expect(container.textContent).toContain(
      "tmux itself does not control font size",
    );
    expect(container.textContent).toContain("Terminal settings");
    expect(container.textContent).toContain("terminal app");
  });

  it("status_bar select emits tmux.status_bar with the new mode", () => {
    const { onSaveField, container } = mount({
      status_bar: "auto",
      mouse: "auto",
    });
    const selects = container.querySelectorAll("select");
    fireEvent.change(selects[0], { target: { value: "enabled" } });
    expect(onSaveField).toHaveBeenCalledWith(
      "tmux",
      "status_bar",
      "enabled",
    );
  });

  it("mouse select emits tmux.mouse with the new mode", () => {
    const { onSaveField, container } = mount({
      status_bar: "auto",
      mouse: "auto",
    });
    const selects = container.querySelectorAll("select");
    fireEvent.change(selects[1], { target: { value: "disabled" } });
    expect(onSaveField).toHaveBeenCalledWith("tmux", "mouse", "disabled");
  });

  it("each mode option round-trips through status_bar", () => {
    for (const mode of ["auto", "enabled", "disabled"] as const) {
      const { onSaveField, container } = mount({
        status_bar: "auto",
        mouse: "auto",
      });
      const select = container.querySelectorAll("select")[0];
      fireEvent.change(select, { target: { value: mode } });
      expect(onSaveField).toHaveBeenCalledWith("tmux", "status_bar", mode);
    }
  });

  it("onUpdate merges patch into existing tmux state", () => {
    const { onUpdate, container } = mount({
      status_bar: "auto",
      mouse: "enabled",
      history_limit: 2000,
    });
    const select = container.querySelectorAll("select")[0];
    fireEvent.change(select, { target: { value: "disabled" } });
    expect(onUpdate).toHaveBeenCalledWith({
      tmux: { status_bar: "disabled", mouse: "enabled", history_limit: 2000 },
    });
  });

  it("defaults to 'auto' when fields are absent on initial mount", () => {
    const { container } = mount({});
    const selects = container.querySelectorAll(
      "select",
    ) as NodeListOf<HTMLSelectElement>;
    expect(selects[0].value).toBe("auto");
    expect(selects[1].value).toBe("auto");
  });

  it("defaults history_limit to 2000 when absent on initial mount", () => {
    const { container } = mount({});
    const input = container.querySelector(
      'input[type="number"]',
    ) as HTMLInputElement;

    expect(input.value).toBe("2000");
  });

  it("history_limit emits tmux.history_limit with a normalized integer", () => {
    const { onSaveField, onUpdate, container } = mount({
      status_bar: "auto",
      mouse: "enabled",
      history_limit: 2000,
    });
    const input = container.querySelector(
      'input[type="number"]',
    ) as HTMLInputElement;

    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "50000.8" } });
    fireEvent.blur(input);

    expect(onSaveField).toHaveBeenCalledWith("tmux", "history_limit", 50000);
    expect(onUpdate).toHaveBeenCalledWith({
      tmux: { status_bar: "auto", mouse: "enabled", history_limit: 50000 },
    });
  });

  it("history_limit clamps to the maximum supported value", () => {
    const { onSaveField, container } = mount({
      status_bar: "auto",
      mouse: "enabled",
      history_limit: 2000,
    });
    const input = container.querySelector(
      'input[type="number"]',
    ) as HTMLInputElement;

    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "999999" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(onSaveField).toHaveBeenCalledWith(
      "tmux",
      "history_limit",
      200000,
    );
  });
});
