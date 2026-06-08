// @vitest-environment jsdom
//
// Rendering + interaction tests for the structured view model + reasoning
// effort pickers (#1403). Covers:
//   - render shape per category (filter on category, label
//     truncation, pending affordance),
//   - effort widget adaptive switch (segmented vs dropdown),
//   - click invokes the callback with (config_id, value) and not the
//     option's display name,
//   - hidden chrome when the adapter advertises neither category,
//   - non-blocking switch-failed notice renders + dismisses.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import {
  ConfigOptionSwitchFailedNotice,
  SessionConfigControls,
} from "./SessionConfigControls";
import type { ConfigOptionDescriptor } from "../../lib/acpTypes";

afterEach(() => {
  cleanup();
});

function modelOption(): ConfigOptionDescriptor {
  return {
    id: "model",
    name: "Model",
    category: "model",
    current_value: "claude-opus-4-7",
    options: [
      { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
      { value: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
    ],
  };
}

function effortOption(): ConfigOptionDescriptor {
  return {
    id: "effort",
    name: "Reasoning Effort",
    category: "thought_level",
    current_value: "default",
    options: [
      { value: "default", name: "Default" },
      { value: "low", name: "Low" },
      { value: "medium", name: "Medium" },
      { value: "high", name: "High" },
    ],
  };
}

describe("SessionConfigControls", () => {
  it("renders nothing when adapter advertises neither category", () => {
    const { container } = render(
      <SessionConfigControls
        configOptions={[]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders only the model dropdown when no effort option exists", () => {
    render(
      <SessionConfigControls
        configOptions={[modelOption()]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    expect(screen.getByTestId("config-option-model")).toBeTruthy();
    expect(screen.queryByTestId("config-option-effort")).toBeNull();
  });

  it("renders only the effort segmented control when no model option exists", () => {
    render(
      <SessionConfigControls
        configOptions={[effortOption()]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    expect(screen.getByTestId("config-option-effort")).toBeTruthy();
    expect(screen.queryByTestId("config-option-model")).toBeNull();
  });

  it("renders the effort options as a segmented radiogroup for short lists", () => {
    render(
      <SessionConfigControls
        configOptions={[effortOption()]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    const group = screen.getByRole("radiogroup", { name: "Reasoning Effort" });
    expect(group).toBeTruthy();
    expect(screen.getByText("Default")).toBeTruthy();
    expect(screen.getByText("High")).toBeTruthy();
  });

  it("falls back from segmented to dropdown when the effort list is too long", () => {
    const sixOptions: ConfigOptionDescriptor = {
      ...effortOption(),
      options: [
        { value: "default", name: "Default" },
        { value: "low", name: "Low" },
        { value: "medium", name: "Medium" },
        { value: "high", name: "High" },
        { value: "very_high", name: "Very High" },
        { value: "extreme", name: "Extreme reasoning" },
      ],
    };
    render(
      <SessionConfigControls
        configOptions={[sixOptions]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    // > 5 options trips the threshold; dropdown is rendered (no
    // radiogroup) and a single chip is shown for the current value.
    expect(screen.queryByRole("radiogroup")).toBeNull();
    expect(screen.getByTestId("config-option-effort")).toBeTruthy();
  });

  it("model trigger exposes aria-expanded + aria-controls toggling open state", () => {
    render(
      <SessionConfigControls
        configOptions={[modelOption()]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    const chip = screen.getByTestId("config-option-model");
    expect(chip.getAttribute("aria-haspopup")).toBe("menu");
    expect(chip.getAttribute("aria-expanded")).toBe("false");
    expect(chip.getAttribute("aria-controls")).toBeNull();
    fireEvent.click(chip);
    expect(chip.getAttribute("aria-expanded")).toBe("true");
    expect(chip.getAttribute("aria-controls")).toBe("config-option-menu-model");
    expect(document.getElementById("config-option-menu-model")).not.toBeNull();
  });

  it("clicking a model option invokes onSetConfigOption with config_id and value", () => {
    const fn = vi.fn();
    render(
      <SessionConfigControls
        configOptions={[modelOption()]}
        pendingConfigOption={null}
        onSetConfigOption={fn}
      />,
    );
    fireEvent.click(screen.getByTestId("config-option-model"));
    fireEvent.click(
      screen.getByTestId("config-option-model-value-claude-sonnet-4-6"),
    );
    expect(fn).toHaveBeenCalledTimes(1);
    expect(fn).toHaveBeenCalledWith("model", "claude-sonnet-4-6");
  });

  it("clicking an effort segment invokes onSetConfigOption with the value (not the label)", () => {
    const fn = vi.fn();
    render(
      <SessionConfigControls
        configOptions={[effortOption()]}
        pendingConfigOption={null}
        onSetConfigOption={fn}
      />,
    );
    fireEvent.click(screen.getByTestId("config-option-effort-value-high"));
    expect(fn).toHaveBeenCalledWith("effort", "high");
  });

  it("disables only the pending option in the dropdown", () => {
    render(
      <SessionConfigControls
        configOptions={[modelOption()]}
        pendingConfigOption={{
          configId: "model",
          value: "claude-sonnet-4-6",
        }}
        onSetConfigOption={vi.fn()}
      />,
    );
    fireEvent.click(screen.getByTestId("config-option-model"));
    const pending = screen.getByTestId(
      "config-option-model-value-claude-sonnet-4-6",
    ) as HTMLButtonElement;
    const other = screen.getByTestId(
      "config-option-model-value-claude-opus-4-7",
    ) as HTMLButtonElement;
    expect(pending.disabled).toBe(true);
    expect(other.disabled).toBe(false);
  });

  // #1562: an unknown category arrives on the wire as a bare string
  // (the Rust `Other(String)` arm is `#[serde(untagged)]`). The picker
  // filters by string equality, so an unknown-category option must not
  // break the model / effort lookup and gets no widget of its own.
  it("ignores an unknown-category option and still finds the known ones", () => {
    const unknown: ConfigOptionDescriptor = {
      id: "future",
      name: "Future Selector",
      category: "future_category",
      current_value: "a",
      options: [{ value: "a", name: "A" }],
    };
    render(
      <SessionConfigControls
        configOptions={[unknown, modelOption(), effortOption()]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    expect(screen.getByTestId("config-option-model")).toBeTruthy();
    expect(screen.getByTestId("config-option-effort")).toBeTruthy();
    expect(screen.queryByTestId("config-option-future")).toBeNull();
  });

  it("renders nothing when only an unknown-category option is present", () => {
    const unknown: ConfigOptionDescriptor = {
      id: "future",
      name: "Future Selector",
      category: "future_category",
      current_value: "a",
      options: [{ value: "a", name: "A" }],
    };
    const { container } = render(
      <SessionConfigControls
        configOptions={[unknown]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("truncates long model labels in the chip", () => {
    const longModel: ConfigOptionDescriptor = {
      ...modelOption(),
      current_value: "long",
      options: [
        {
          value: "long",
          name: "A Very Long Model Name That Does Not Fit Inline",
        },
      ],
    };
    render(
      <SessionConfigControls
        configOptions={[longModel]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
      />,
    );
    const chip = screen.getByTestId("config-option-model");
    // truncate() preserves the trailing ellipsis on overflow; assert
    // we see it on the chip (not the menu items).
    expect(chip.textContent ?? "").toContain("…");
  });
});

describe("ConfigOptionSwitchFailedNotice", () => {
  it("renders nothing when there is no failure", () => {
    const { container } = render(
      <ConfigOptionSwitchFailedNotice
        failure={null}
        configOptions={[]}
        onDismiss={vi.fn()}
      />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders the configured label and the rejection reason", () => {
    render(
      <ConfigOptionSwitchFailedNotice
        failure={{
          configId: "model",
          value: "claude-sonnet-4-6",
          reason: "rate limited",
          at: new Date().toISOString(),
        }}
        configOptions={[modelOption()]}
        onDismiss={vi.fn()}
      />,
    );
    const notice = screen.getByTestId("config-option-switch-failed-notice");
    expect(notice.textContent ?? "").toContain("Model");
    expect(notice.textContent ?? "").toContain("Claude Sonnet 4.6");
    expect(notice.textContent ?? "").toContain("rate limited");
  });

  it("invokes onDismiss when the dismiss button is clicked", () => {
    const fn = vi.fn();
    render(
      <ConfigOptionSwitchFailedNotice
        failure={{
          configId: "model",
          value: "claude-sonnet-4-6",
          reason: "rate limited",
          at: new Date().toISOString(),
        }}
        configOptions={[modelOption()]}
        onDismiss={fn}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Dismiss notice" }));
    expect(fn).toHaveBeenCalledTimes(1);
  });
});
