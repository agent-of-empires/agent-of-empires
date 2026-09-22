// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ConfigOptionSwitchFailedNotice, SessionConfigControls } from "./SessionConfigControls";
import type { AcpState, ConfigOptionDescriptor } from "../../lib/acpTypes";

afterEach(() => {
  cleanup();
});

const option = (id: string, name: string, category: string, names: string[]): ConfigOptionDescriptor => ({
  id,
  name,
  category,
  current_value: names[0]!.toLowerCase().replace(/ /g, "_"),
  options: names.map((n) => ({ value: n.toLowerCase().replace(/ /g, "_"), name: n })),
});
const MODEL: ConfigOptionDescriptor = {
  ...option("model", "Model", "model", ["x"]),
  current_value: "claude-opus-4-7",
  options: [
    { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
    { value: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
  ],
};
const EFFORT = option("effort", "Reasoning Effort", "thought_level", ["Default", "Low", "Medium", "High"]);
// An unknown category arrives as a bare string and gets no widget.
const UNKNOWN = option("future", "Future Selector", "future_category", ["A"]);

function mount(
  configOptions: ConfigOptionDescriptor[],
  pendingConfigOption: AcpState["pendingConfigOption"] = null,
  onSetConfigOption = vi.fn(),
) {
  const utils = render(
    <SessionConfigControls
      configOptions={configOptions}
      pendingConfigOption={pendingConfigOption}
      onSetConfigOption={onSetConfigOption}
    />,
  );
  return { ...utils, onSetConfigOption };
}

const byId = (id: string) => screen.queryByTestId(`config-option-${id}`);

describe("SessionConfigControls", () => {
  it.each([
    [[], []],
    [[UNKNOWN], []],
    [[MODEL], ["model"]],
    [[EFFORT], ["effort"]],
    [
      [UNKNOWN, MODEL, EFFORT],
      ["model", "effort"],
    ],
  ])("renders widgets for %#", (options, shown) => {
    const { container } = mount(options);
    if (shown.length === 0) expect(container.firstChild).toBeNull();
    for (const id of ["model", "effort", "future"]) expect(byId(id) !== null).toBe(shown.includes(id));
  });

  it("uses a segmented control for short effort lists and a dropdown past the threshold", () => {
    mount([EFFORT]);
    expect(screen.getByRole("radiogroup", { name: "Reasoning Effort" })).toBeTruthy();
    expect(screen.getByText("High")).toBeTruthy();
    cleanup();
    mount([
      option("effort", "Reasoning Effort", "thought_level", [
        "Default",
        "Low",
        "Medium",
        "High",
        "Very High",
        "Extreme reasoning",
      ]),
    ]);
    expect(screen.queryByRole("radiogroup")).toBeNull();
    expect(byId("effort")).toBeTruthy();
  });

  it("toggles the model menu aria state and sends the option value", () => {
    const { onSetConfigOption } = mount([MODEL]);
    const chip = byId("model")!;
    expect(chip.getAttribute("aria-haspopup")).toBe("menu");
    expect(chip.getAttribute("aria-expanded")).toBe("false");
    expect(chip.getAttribute("aria-controls")).toBeNull();
    fireEvent.click(chip);
    expect(chip.getAttribute("aria-expanded")).toBe("true");
    expect(chip.getAttribute("aria-controls")).toBe("config-option-menu-model");
    fireEvent.click(byId("model-value-claude-sonnet-4-6")!);
    expect(onSetConfigOption).toHaveBeenCalledExactlyOnceWith("model", "claude-sonnet-4-6");
  });

  it("sends the effort value, not its label", () => {
    const { onSetConfigOption } = mount([EFFORT]);
    fireEvent.click(byId("effort-value-high")!);
    expect(onSetConfigOption).toHaveBeenCalledWith("effort", "high");
  });

  it("disables only the pending option", () => {
    mount([MODEL], { configId: "model", value: "claude-sonnet-4-6" });
    fireEvent.click(byId("model")!);
    expect((byId("model-value-claude-sonnet-4-6") as HTMLButtonElement).disabled).toBe(true);
    expect((byId("model-value-claude-opus-4-7") as HTMLButtonElement).disabled).toBe(false);
  });

  it("truncates long model labels in the chip", () => {
    mount([
      {
        ...MODEL,
        current_value: "long",
        options: [{ value: "long", name: "A Very Long Model Name That Does Not Fit Inline" }],
      },
    ]);
    expect(byId("model")!.textContent).toContain("…");
  });

  // Up when a floor's worth of room exists above, else the roomier side, clamped to what is visible.
  it.each([
    ["ample room above", 400, 420, 800, undefined, 0, "up", 288],
    ["cramped above, ample below", 50, 60, 800, undefined, 0, "down", 288],
    ["prefers up once the floor clears, even with more room below", 200, 220, 800, undefined, 0, "up", 192],
    ["cramped both ways, above larger", 50, 60, 100, undefined, 0, "up", 42],
    ["cramped both ways, below larger", 20, 30, 100, undefined, 0, "down", 62],
    ["exact tie resolves to up", 58, 68, 126, undefined, 0, "up", 50],
    // A zoom offset shifts the visible top, leaving too little room above.
    ["visualViewport offset flips the direction", 250, 270, 1000, 800, 200, "down", 288],
  ])("menu layout: %s", (_label, top, bottom, innerHeight, vvHeight, vvOffsetTop, direction, maxHeight) => {
    const restore = [
      ["innerHeight", Object.getOwnPropertyDescriptor(window, "innerHeight")],
      ["visualViewport", Object.getOwnPropertyDescriptor(window, "visualViewport")],
    ] as const;
    const rectSpy = vi.spyOn(Element.prototype, "getBoundingClientRect");
    try {
      Object.defineProperty(window, "innerHeight", { value: innerHeight, configurable: true, writable: true });
      Object.defineProperty(window, "visualViewport", {
        value:
          vvHeight == null
            ? undefined
            : { height: vvHeight, offsetTop: vvOffsetTop, addEventListener: vi.fn(), removeEventListener: vi.fn() },
        configurable: true,
        writable: true,
      });
      rectSpy.mockReturnValue({
        top,
        bottom,
        left: 0,
        right: 0,
        width: 0,
        height: bottom - top,
        x: 0,
        y: top,
        toJSON: () => ({}),
      } as DOMRect);
      mount([MODEL]);
      fireEvent.click(byId("model")!);
      const menu = document.getElementById("config-option-menu-model")!;
      expect(menu.className).toContain(direction === "up" ? "bottom-full" : "top-full");
      expect(menu.style.maxHeight).toBe(`${maxHeight}px`);
    } finally {
      rectSpy.mockRestore();
      for (const [key, descriptor] of restore) {
        if (descriptor) Object.defineProperty(window, key, descriptor);
        else delete (window as unknown as Record<string, unknown>)[key];
      }
    }
  });
});

describe("ConfigOptionSwitchFailedNotice", () => {
  it("renders nothing without a failure", () => {
    const { container } = render(
      <ConfigOptionSwitchFailedNotice failure={null} configOptions={[]} onDismiss={vi.fn()} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("names the config and option, shows the reason, and dismisses", () => {
    const onDismiss = vi.fn();
    render(
      <ConfigOptionSwitchFailedNotice
        failure={{
          configId: "model",
          value: "claude-sonnet-4-6",
          reason: "rate limited",
          at: new Date().toISOString(),
        }}
        configOptions={[MODEL]}
        onDismiss={onDismiss}
      />,
    );
    const text = screen.getByTestId("config-option-switch-failed-notice").textContent;
    for (const s of ["Model", "Claude Sonnet 4.6", "rate limited"]) expect(text).toContain(s);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss notice" }));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });
});
