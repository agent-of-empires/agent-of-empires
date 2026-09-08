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

import { ConfigOptionSwitchFailedNotice, SessionConfigControls } from "./SessionConfigControls";
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
      <SessionConfigControls configOptions={[]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders only the model dropdown when no effort option exists", () => {
    render(
      <SessionConfigControls configOptions={[modelOption()]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
    );
    expect(screen.getByTestId("config-option-model")).toBeTruthy();
    expect(screen.queryByTestId("config-option-effort")).toBeNull();
  });

  it("renders only the effort segmented control when no model option exists", () => {
    render(
      <SessionConfigControls configOptions={[effortOption()]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
    );
    expect(screen.getByTestId("config-option-effort")).toBeTruthy();
    expect(screen.queryByTestId("config-option-model")).toBeNull();
  });

  it("renders the effort options as a segmented radiogroup for short lists", () => {
    render(
      <SessionConfigControls configOptions={[effortOption()]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
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
      <SessionConfigControls configOptions={[sixOptions]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
    );
    // > 5 options trips the threshold; dropdown is rendered (no
    // radiogroup) and a single chip is shown for the current value.
    expect(screen.queryByRole("radiogroup")).toBeNull();
    expect(screen.getByTestId("config-option-effort")).toBeTruthy();
  });

  it("model trigger exposes aria-expanded + aria-controls toggling open state", () => {
    render(
      <SessionConfigControls configOptions={[modelOption()]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
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
    render(<SessionConfigControls configOptions={[modelOption()]} pendingConfigOption={null} onSetConfigOption={fn} />);
    fireEvent.click(screen.getByTestId("config-option-model"));
    fireEvent.click(screen.getByTestId("config-option-model-value-claude-sonnet-4-6"));
    expect(fn).toHaveBeenCalledTimes(1);
    expect(fn).toHaveBeenCalledWith("model", "claude-sonnet-4-6");
  });

  it("clicking an effort segment invokes onSetConfigOption with the value (not the label)", () => {
    const fn = vi.fn();
    render(
      <SessionConfigControls configOptions={[effortOption()]} pendingConfigOption={null} onSetConfigOption={fn} />,
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
    const pending = screen.getByTestId("config-option-model-value-claude-sonnet-4-6") as HTMLButtonElement;
    const other = screen.getByTestId("config-option-model-value-claude-opus-4-7") as HTMLButtonElement;
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
      <SessionConfigControls configOptions={[unknown]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
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
      <SessionConfigControls configOptions={[longModel]} pendingConfigOption={null} onSetConfigOption={vi.fn()} />,
    );
    const chip = screen.getByTestId("config-option-model");
    // truncate() preserves the trailing ellipsis on overflow; assert
    // we see it on the chip (not the menu items).
    expect(chip.textContent ?? "").toContain("…");
  });
});

describe("ModelDropdown menu direction and height", () => {
  // The menu prefers opening upward, sized by the trigger's distance to
  // the viewport top, but flips downward (sized by the distance to the
  // viewport bottom) when there isn't at least floor-height room above.
  // When neither direction clears the floor, it picks whichever has more
  // room and clamps to exactly that, rather than forcing a fixed floor
  // height that would clip off-screen. See `computeMenuLayout`.
  it("picks direction and max-height from the trigger's actual room above/below", () => {
    const cases: Array<{
      name: string;
      rectTop: number;
      rectBottom: number;
      innerHeight: number;
      vvHeight?: number;
      vvOffsetTop?: number;
      direction: "up" | "down";
      maxHeightPx: number;
    }> = [
      { name: "ample room above", rectTop: 400, rectBottom: 420, innerHeight: 800, direction: "up", maxHeightPx: 288 },
      {
        name: "cramped above, ample below",
        rectTop: 50,
        rectBottom: 60,
        innerHeight: 800,
        direction: "down",
        maxHeightPx: 288,
      },
      {
        // Above clears the floor (192px) even though below has far more
        // room (572px): proves the choice is floor-based preference for
        // "up", not "pick whichever side has more room".
        name: "clears the floor above, prefers up over a much larger below",
        rectTop: 200,
        rectBottom: 220,
        innerHeight: 800,
        direction: "up",
        maxHeightPx: 192,
      },
      {
        name: "cramped both ways, above larger (not a tie)",
        rectTop: 50,
        rectBottom: 60,
        innerHeight: 100,
        direction: "up",
        maxHeightPx: 42,
      },
      {
        name: "cramped both ways, below larger",
        rectTop: 20,
        rectBottom: 30,
        innerHeight: 100,
        direction: "down",
        maxHeightPx: 62,
      },
      {
        // Exact tie (spaceAbove === spaceBelow, both below the floor):
        // neither the floor check nor the `spaceBelow > spaceAbove`
        // strict inequality fires, so the fallthrough resolves to "up".
        name: "cramped both ways, exact tie resolves to up",
        rectTop: 58,
        rectBottom: 68,
        innerHeight: 126,
        direction: "up",
        maxHeightPx: 50,
      },
      {
        // visualViewport.offsetTop shifts where the visible area actually
        // starts (e.g. after pinch-zoom), so it must shift both edges of
        // the visible interval, not just its size. Ignoring it here (i.e.
        // treating the visible top as layout y=0) would compute spaceAbove
        // as 242px (>= floor) and wrongly pick "up"; correctly offsetting
        // by 200px puts the trigger only 42px below the true visible top
        // (< floor), so it correctly flips to "down", where 722px is
        // available (clamped to the 288px cap).
        name: "non-zero visualViewport.offsetTop flips the chosen direction",
        rectTop: 250,
        rectBottom: 270,
        innerHeight: 1000,
        vvHeight: 800,
        vvOffsetTop: 200,
        direction: "down",
        maxHeightPx: 288,
      },
    ];

    const originalInnerHeightDescriptor = Object.getOwnPropertyDescriptor(window, "innerHeight");
    const originalVisualViewportDescriptor = Object.getOwnPropertyDescriptor(window, "visualViewport");
    const rectSpy = vi.spyOn(Element.prototype, "getBoundingClientRect");

    try {
      for (const c of cases) {
        Object.defineProperty(window, "innerHeight", { value: c.innerHeight, configurable: true, writable: true });
        Object.defineProperty(window, "visualViewport", {
          value:
            c.vvHeight == null
              ? undefined
              : {
                  height: c.vvHeight,
                  offsetTop: c.vvOffsetTop ?? 0,
                  addEventListener: vi.fn(),
                  removeEventListener: vi.fn(),
                },
          configurable: true,
          writable: true,
        });
        rectSpy.mockReturnValue({
          top: c.rectTop,
          bottom: c.rectBottom,
          left: 0,
          right: 0,
          width: 0,
          height: c.rectBottom - c.rectTop,
          x: 0,
          y: c.rectTop,
          toJSON: () => ({}),
        } as DOMRect);

        const { unmount } = render(
          <SessionConfigControls
            configOptions={[modelOption()]}
            pendingConfigOption={null}
            onSetConfigOption={vi.fn()}
          />,
        );
        fireEvent.click(screen.getByTestId("config-option-model"));
        const menu = document.getElementById("config-option-menu-model");
        expect(menu, c.name).not.toBeNull();
        expect(menu!.className.includes(c.direction === "up" ? "bottom-full" : "top-full"), c.name).toBe(true);
        expect(menu!.style.maxHeight, c.name).toBe(`${c.maxHeightPx}px`);

        unmount();
      }
    } finally {
      rectSpy.mockRestore();
      if (originalInnerHeightDescriptor) {
        Object.defineProperty(window, "innerHeight", originalInnerHeightDescriptor);
      }
      if (originalVisualViewportDescriptor) {
        Object.defineProperty(window, "visualViewport", originalVisualViewportDescriptor);
      } else {
        delete (window as unknown as { visualViewport?: unknown }).visualViewport;
      }
    }
  });
});

describe("ConfigOptionSwitchFailedNotice", () => {
  it("renders nothing when there is no failure", () => {
    const { container } = render(
      <ConfigOptionSwitchFailedNotice failure={null} configOptions={[]} onDismiss={vi.fn()} />,
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
