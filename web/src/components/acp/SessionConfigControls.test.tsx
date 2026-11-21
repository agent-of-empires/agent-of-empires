// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import {
  ConfigOptionSwitchFailedNotice,
  SessionConfigControls,
  type ModelDefaultTarget,
} from "./SessionConfigControls";
import type { AcpState, ConfigOptionDescriptor } from "../../lib/acpTypes";
import { toastBus, type ToastApi } from "../../lib/toastBus";

const defaults = vi.hoisted(() => ({ loadAgentModelDefault: vi.fn(), saveAgentModelDefault: vi.fn() }));
vi.mock("../../lib/agentModelDefault", () => defaults);

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
  provider?: { current?: string | null; pending?: string | null; onSet?: () => void },
) {
  const utils = render(
    <SessionConfigControls
      configOptions={configOptions}
      pendingConfigOption={pendingConfigOption}
      onSetConfigOption={onSetConfigOption}
      provider={provider?.current ?? null}
      providerPending={provider?.pending ?? null}
      onSetProvider={provider?.onSet}
    />,
  );
  return { ...utils, onSetConfigOption };
}

const toggle = (id: string, name: string, current: "on" | "off"): ConfigOptionDescriptor => ({
  ...option(id, name, "model_config", ["On", "Off"]),
  current_value: current,
});
const FAST = toggle("fast", "Fast mode", "off");
const ULTRACODE = toggle("ultracode", "Ultracode", "on");

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

  it("renders only on/off model_config options as switches that send the opposite value", () => {
    const notOnOff = option("tier", "Tier", "model_config", ["Standard", "Priority"]);
    const wrongCategory = { ...toggle("beta", "Beta", "off"), category: "future_category" };
    const { onSetConfigOption } = mount([MODEL, FAST, ULTRACODE, notOnOff, wrongCategory]);

    expect(screen.getAllByRole("switch").map((el) => el.getAttribute("aria-label"))).toEqual([
      "Fast mode",
      "Ultracode",
    ]);
    expect(screen.getByRole("switch", { name: "Fast mode" }).getAttribute("aria-checked")).toBe("false");
    expect(screen.getByRole("switch", { name: "Ultracode" }).getAttribute("aria-checked")).toBe("true");

    fireEvent.click(screen.getByRole("switch", { name: "Fast mode" }));
    fireEvent.click(screen.getByRole("switch", { name: "Ultracode" }));
    expect(onSetConfigOption.mock.calls).toEqual([
      ["fast", "on"],
      ["ultracode", "off"],
    ]);
  });

  it("disables a switch while its flip is pending and keeps the confirmed state", () => {
    mount([FAST], { configId: "fast", value: "on" });
    const sw = screen.getByRole("switch", { name: "Fast mode" }) as HTMLButtonElement;
    expect(sw.disabled).toBe(true);
    expect(sw.getAttribute("aria-checked")).toBe("false");
  });

  it("drops the switch when the adapter stops advertising the option", () => {
    const { rerender, onSetConfigOption } = mount([FAST]);
    expect(screen.queryByRole("switch")).not.toBeNull();
    rerender(
      <SessionConfigControls configOptions={[]} pendingConfigOption={null} onSetConfigOption={onSetConfigOption} />,
    );
    expect(screen.queryByRole("switch")).toBeNull();
  });

  it("uses a segmented control for short effort lists, sending the value, and a dropdown past the threshold", () => {
    const { onSetConfigOption } = mount([EFFORT]);
    expect(screen.getByRole("radiogroup", { name: "Reasoning Effort" })).toBeTruthy();
    fireEvent.click(byId("effort-value-high")!);
    expect(onSetConfigOption).toHaveBeenCalledWith("effort", "high");
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
    fireEvent.click(byId("effort")!);
    expect(screen.queryByRole("textbox", { name: "Search models" })).toBeNull();
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

  it("disables only the pending option", () => {
    const { onSetConfigOption } = mount([MODEL], { configId: "model", value: "claude-sonnet-4-6" });
    fireEvent.click(byId("model")!);
    expect((byId("model-value-claude-sonnet-4-6") as HTMLButtonElement).disabled).toBe(true);
    expect((byId("model-value-claude-opus-4-7") as HTMLButtonElement).disabled).toBe(false);
    fireEvent.change(screen.getByRole("textbox", { name: "Search models" }), { target: { value: "Sonnet" } });
    fireEvent.click(byId("model-value-claude-sonnet-4-6")!);
    expect(onSetConfigOption).not.toHaveBeenCalled();
    expect(byId("model")!.getAttribute("aria-expanded")).toBe("true");
  });

  it("filters by model name and provider/model id, sends the advertised value, and resets on reopen", () => {
    const models: ConfigOptionDescriptor = {
      ...MODEL,
      options: [
        ...MODEL.options,
        { value: "stepcode-openai/gpt-6.1-sol-sys-paygo", name: "GPT 6.1 Sol" },
        { value: "stepcode/deepseek-v4-flash-wandb", name: "DeepSeek V4 Flash" },
      ],
    };
    const { onSetConfigOption } = mount([models]);
    fireEvent.click(byId("model")!);
    const search = screen.getByRole("textbox", { name: "Search models" });
    for (const [query, values] of [
      ["sOnNeT", ["claude-sonnet-4-6"]],
      ["  STEPCODE-OPENAI   sol  ", ["stepcode-openai/gpt-6.1-sol-sys-paygo"]],
      ["deepseek-v4", ["stepcode/deepseek-v4-flash-wandb"]],
      ["no-such-model", []],
      ["   ", models.options.map((opt) => opt.value)],
    ] as const) {
      fireEvent.change(search, { target: { value: query } });
      const items = screen.queryAllByRole("menuitem");
      expect(items.map((item) => item.getAttribute("data-testid"))).toEqual(
        values.map((value) => `config-option-model-value-${value}`),
      );
      expect(screen.queryByRole("status") !== null).toBe(values.length === 0);
      expect(onSetConfigOption).not.toHaveBeenCalled();
    }
    fireEvent.change(search, { target: { value: "stepcode-openai" } });
    fireEvent.click(byId("model-value-stepcode-openai/gpt-6.1-sol-sys-paygo")!);
    expect(onSetConfigOption).toHaveBeenCalledExactlyOnceWith("model", "stepcode-openai/gpt-6.1-sol-sys-paygo");
    expect(byId("model")!.textContent).toContain("Claude Opus 4.7");
    fireEvent.click(byId("model")!);
    expect((screen.getByRole("textbox", { name: "Search models" }) as HTMLInputElement).value).toBe("");
    expect(screen.getAllByRole("menuitem")).toHaveLength(models.options.length);
  });

  // Selecting unmounts whatever held focus, so keyboard paths hand it back to the chip (#4894).
  it("returns focus to the chip after keyboard selection and leaves pointer clicks alone", () => {
    const { onSetConfigOption } = mount([MODEL]);
    const chip = byId("model")!;
    fireEvent.click(chip);
    const search = screen.getByRole("textbox", { name: "Search models" });
    fireEvent.change(search, { target: { value: "sonnet" } });
    fireEvent.keyDown(search, { key: "Enter" });
    expect(onSetConfigOption).toHaveBeenCalledExactlyOnceWith("model", "claude-sonnet-4-6");
    expect(document.activeElement).toBe(chip);
    // A pointer click reports its click count; focus keeps following the browser's own rules.
    fireEvent.click(chip);
    const sonnet = byId("model-value-claude-sonnet-4-6") as HTMLButtonElement;
    sonnet.focus();
    fireEvent.click(sonnet, { detail: 1 });
    expect(document.activeElement).not.toBe(chip);
  });

  it("keeps the picker open when Escape cancels an IME composition", () => {
    mount([MODEL]);
    const chip = byId("model")!;
    fireEvent.click(chip);
    const search = screen.getByRole("textbox", { name: "Search models" });
    fireEvent.keyDown(search, { key: "Escape", isComposing: true });
    expect(chip.getAttribute("aria-expanded")).toBe("true");
    fireEvent.keyDown(search, { key: "Escape" });
    expect(chip.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(chip);
  });

  // Safari ends the composition first, so the committing key arrives as keyCode 229 with isComposing false.
  it("ignores a composition-committing key that only reports keyCode 229", () => {
    const { onSetConfigOption } = mount([MODEL]);
    const chip = byId("model")!;
    fireEvent.click(chip);
    const search = screen.getByRole("textbox", { name: "Search models" });
    fireEvent.change(search, { target: { value: "sonnet" } });
    fireEvent.keyDown(search, { key: "Enter", keyCode: 229 });
    expect(onSetConfigOption).not.toHaveBeenCalled();
    expect(chip.getAttribute("aria-expanded")).toBe("true");
    // The real Enter that follows still selects.
    fireEvent.keyDown(search, { key: "Enter" });
    expect(onSetConfigOption).toHaveBeenCalledExactlyOnceWith("model", "claude-sonnet-4-6");
  });

  it("distinguishes an advertised model list with no options from a search with no matches", () => {
    mount([{ ...MODEL, options: [] }]);
    fireEvent.click(byId("model")!);
    expect(screen.getByRole("status").textContent).toBe("No models available.");
    fireEvent.change(screen.getByRole("textbox", { name: "Search models" }), { target: { value: "opus" } });
    expect(screen.getByRole("status").textContent).toBe("No models match your search.");
  });

  // Up when a floor's worth of room exists above, else the roomier side, clamped to what is visible (#3747).
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
  it("renders nothing without a failure; otherwise names the option, shows the reason, and dismisses", () => {
    const onDismiss = vi.fn();
    const props = { configOptions: [MODEL], onDismiss };
    const { container, rerender } = render(<ConfigOptionSwitchFailedNotice failure={null} {...props} />);
    expect(container.firstChild).toBeNull();
    rerender(
      <ConfigOptionSwitchFailedNotice
        failure={{
          configId: "model",
          value: "claude-sonnet-4-6",
          reason: "rate limited",
          at: new Date().toISOString(),
        }}
        {...props}
      />,
    );
    const text = screen.getByTestId("config-option-switch-failed-notice").textContent;
    for (const s of ["Model", "Claude Sonnet 4.6", "rate limited"]) expect(text).toContain(s);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss notice" }));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });
});

describe("provider picker", () => {
  it("closes an open picker when a turn starts and cannot submit its previous choices", () => {
    const onSet = vi.fn();
    const props = {
      configOptions: [],
      pendingConfigOption: null,
      onSetConfigOption: vi.fn(),
      provider: "api",
      onSetProvider: onSet,
    };
    const { rerender } = render(<SessionConfigControls {...props} />);
    fireEvent.click(screen.getByTestId("config-option-aoe-provider"));
    expect(screen.getByTestId("config-option-aoe-provider-value-vertex")).toBeTruthy();
    rerender(<SessionConfigControls {...props} providerLockedReason="turn active" />);
    const choice = screen.queryByTestId("config-option-aoe-provider-value-vertex");
    if (choice) fireEvent.click(choice);
    expect(onSet).not.toHaveBeenCalled();
    expect(screen.queryByRole("menu")).toBeNull();
    rerender(<SessionConfigControls {...props} />);
    expect(screen.queryByRole("menu")).toBeNull();
  });
  it("is absent for an agent that does not route through a provider", () => {
    const { container } = mount([MODEL]);
    expect(screen.queryByTestId("config-option-aoe-provider")).toBeNull();
    expect(container.firstChild).not.toBeNull();
  });

  it("renders alone for a Claude session with no adapter options yet", () => {
    mount([], null, vi.fn(), { onSet: vi.fn() });
    expect(screen.getByTestId("config-option-aoe-provider")).toBeTruthy();
  });

  it("posts the picked provider", () => {
    const onSet = vi.fn();
    mount([], null, vi.fn(), { current: "api", onSet });
    fireEvent.click(screen.getByTestId("config-option-aoe-provider"));
    expect(screen.queryByRole("textbox", { name: "Search models" })).toBeNull();
    fireEvent.click(screen.getByTestId("config-option-aoe-provider-value-vertex"));
    expect(onSet).toHaveBeenCalledWith("vertex");
  });

  // There is no API for returning to the host default, so the entry naming
  // that state must not be selectable; the dropdown refuses the current value.
  it("does not post the host-default entry", () => {
    const onSet = vi.fn();
    mount([], null, vi.fn(), { current: null, onSet });
    fireEvent.click(screen.getByTestId("config-option-aoe-provider"));
    fireEvent.click(screen.getByTestId("config-option-aoe-provider-value-"));
    expect(onSet).not.toHaveBeenCalled();
  });

  it("keeps the picker closed until its pending request settles", () => {
    const onSet = vi.fn();
    const props = {
      configOptions: [],
      pendingConfigOption: null,
      onSetConfigOption: vi.fn(),
      provider: "api",
      onSetProvider: onSet,
    };
    const { rerender } = render(<SessionConfigControls {...props} providerPending="bedrock" />);
    expect(screen.getByTestId("config-option-aoe-provider").hasAttribute("disabled")).toBe(true);
    fireEvent.click(screen.getByTestId("config-option-aoe-provider"));
    expect(screen.queryByRole("menu")).toBeNull();
    expect(onSet).not.toHaveBeenCalled();
    rerender(<SessionConfigControls {...props} />);
    fireEvent.click(screen.getByTestId("config-option-aoe-provider"));
    fireEvent.click(screen.getByTestId("config-option-aoe-provider-value-vertex"));
    expect(onSet).toHaveBeenCalledWith("vertex");
  });
});

describe("model default", () => {
  const open = vi.fn();
  const target: ModelDefaultTarget = { profile: "work", agent: "claude", agentLabel: "Claude Code", open };
  const LABEL = "Default for new Claude Code Sessions in Structured view";
  const toasts = { infoAction: vi.fn(), error: vi.fn(), action: vi.fn() };
  let resolveLoad: (value: unknown) => void = () => {};

  async function mountWithDefault(loaded: unknown, extra: Partial<Parameters<typeof SessionConfigControls>[0]> = {}) {
    defaults.loadAgentModelDefault.mockReturnValue(new Promise((r) => (resolveLoad = r)));
    toastBus.handler = toasts as unknown as ToastApi;
    const view = render(
      <SessionConfigControls
        configOptions={[MODEL]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
        modelDefault={target}
        {...extra}
      />,
    );
    fireEvent.click(byId("model")!);
    await act(async () => resolveLoad(loaded));
    return view;
  }
  const item = () => screen.queryByRole("menuitemcheckbox", { name: LABEL });

  afterEach(() => {
    vi.clearAllMocks();
    toastBus.handler = null;
  });

  it.each([
    [
      "unchecked when the picked model isn't the default",
      { model: "claude-sonnet-4-6", pinned: false },
      "false",
      false,
    ],
    ["unchecked for every model when the Agent picks its own", { model: null, pinned: false }, "false", false],
    [
      "checked, dimmed and not changeable when it is the default",
      { model: "claude-opus-4-7", pinned: false },
      "true",
      true,
    ],
  ])("is %s", async (_label, loaded, checked, inert) => {
    await mountWithDefault(loaded);
    expect(defaults.loadAgentModelDefault).toHaveBeenCalledWith("work", "claude");
    expect(item()!.getAttribute("aria-checked")).toBe(checked);
    expect(item()!.getAttribute("aria-disabled")).toBe(inert ? "true" : null);
    fireEvent.click(item()!);
    expect(defaults.saveAgentModelDefault).toHaveBeenCalledTimes(inert ? 0 : 1);
  });

  it("can't be checked until the default has loaded, and is left out when it can't load", async () => {
    await mountWithDefault(undefined);
    defaults.loadAgentModelDefault.mockReturnValue(new Promise(() => {}));
    cleanup();
    render(
      <SessionConfigControls
        configOptions={[MODEL]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
        modelDefault={target}
      />,
    );
    fireEvent.click(byId("model")!);
    expect(item()!.getAttribute("aria-disabled")).toBe("true");
    fireEvent.click(item()!);
    expect(defaults.saveAgentModelDefault).not.toHaveBeenCalled();
    cleanup();
    await mountWithDefault(null);
    expect(item()).toBeNull();
  });

  it("saves the picked model, closes the menu, and toasts with a link to Agent defaults", async () => {
    defaults.saveAgentModelDefault.mockResolvedValue("committed");
    await mountWithDefault({ model: "claude-sonnet-4-6", pinned: false });
    fireEvent.click(item()!);
    expect(defaults.saveAgentModelDefault).toHaveBeenCalledWith("work", "claude", "claude-opus-4-7");
    expect(screen.queryByRole("menu")).toBeNull();
    await waitFor(() =>
      expect(toasts.infoAction).toHaveBeenCalledWith(
        "Claude Opus 4.7 is now the default for new Claude Code Sessions in Structured view",
        "Agent defaults ›",
        open,
      ),
    );
  });

  it.each([
    ["rejected", "Failed to save, please try again"],
    ["uncertain", "Could not confirm the save, please check the value"],
  ])("shows the save error and no confirmation when the save is %s", async (outcome, message) => {
    defaults.saveAgentModelDefault.mockResolvedValue(outcome);
    await mountWithDefault({ model: null, pinned: false });
    fireEvent.click(item()!);
    await waitFor(() => expect(toasts.error).toHaveBeenCalledWith(message));
    expect(toasts.infoAction).not.toHaveBeenCalled();
  });

  it("says a pin added since the menu opened refused the save, and shows the pinned link next time", async () => {
    defaults.saveAgentModelDefault.mockResolvedValue("pinned");
    await mountWithDefault({ model: null, pinned: false });
    fireEvent.click(item()!);
    await waitFor(() =>
      expect(toasts.action).toHaveBeenCalledWith(
        "Claude Code's model is pinned in Agent defaults. Nothing was changed.",
        "Agent defaults ›",
        open,
      ),
    );
    expect(toasts.infoAction).not.toHaveBeenCalled();
    expect(toasts.error).not.toHaveBeenCalled();
    defaults.loadAgentModelDefault.mockResolvedValue({ model: "claude-sonnet-4-6", pinned: true });
    fireEvent.click(byId("model")!);
    expect(await screen.findByRole("menuitem", { name: "Pinned in Agent defaults ›" })).toBeTruthy();
  });

  it("shows a pinned default as a link to Agent defaults instead of the checkbox", async () => {
    await mountWithDefault({ model: "claude-opus-4-7", pinned: true });
    expect(item()).toBeNull();
    fireEvent.click(screen.getByRole("menuitem", { name: "Pinned in Agent defaults ›" }));
    expect(open).toHaveBeenCalledOnce();
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it.each([
    { search: "no matching model", query: "no such model", row: "the checkbox", pinned: false },
    { search: "no matching model", query: "no such model", row: "the pinned link", pinned: true },
    { search: "an empty search", query: "", row: "the checkbox", pinned: false },
    { search: "an empty search", query: "", row: "the pinned link", pinned: true },
  ])("Enter with $search doesn't activate $row", async ({ query, pinned }) => {
    const loaded = { model: "claude-sonnet-4-6", pinned };
    const onSetConfigOption = vi.fn();
    await mountWithDefault(loaded, { onSetConfigOption });
    const search = screen.getByRole("textbox", { name: "Search models" });
    fireEvent.change(search, { target: { value: query } });
    fireEvent.keyDown(search, { key: "Enter" });
    expect(defaults.saveAgentModelDefault).not.toHaveBeenCalled();
    expect(open).not.toHaveBeenCalled();
    // No match picks nothing; an empty search keeps the current model.
    expect(onSetConfigOption).not.toHaveBeenCalled();
  });

  it("reloads for a new Agent while the menu is open, and saves for that Agent", async () => {
    const view = await mountWithDefault({ model: "claude-sonnet-4-6", pinned: false });
    defaults.loadAgentModelDefault.mockReturnValue(new Promise((r) => (resolveLoad = r)));
    view.rerender(
      <SessionConfigControls
        configOptions={[MODEL]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
        modelDefault={{ ...target, agent: "codex", agentLabel: "Codex" }}
      />,
    );
    const codex = screen.getByRole("menuitemcheckbox", { name: "Default for new Codex Sessions in Structured view" });
    // Claude's loaded default must not carry over to Codex.
    expect(codex.getAttribute("aria-disabled")).toBe("true");
    expect(defaults.loadAgentModelDefault).toHaveBeenLastCalledWith("work", "codex");
    defaults.saveAgentModelDefault.mockResolvedValue("committed");
    await act(async () => resolveLoad({ model: "gpt-5", pinned: false }));
    await act(async () => fireEvent.click(codex));
    expect(defaults.saveAgentModelDefault).toHaveBeenCalledWith("work", "codex", "claude-opus-4-7");
  });

  it("is reached with the arrow keys", async () => {
    await mountWithDefault({ model: null, pinned: false });
    // Up from the current model wraps to the last item, the default row, and Down wraps back.
    fireEvent.keyDown(screen.getByRole("menu"), { key: "ArrowUp" });
    expect(document.activeElement).toBe(item());
    fireEvent.keyDown(screen.getByRole("menu"), { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getByRole("menuitem", { name: /Claude Opus 4.7/ }));
  });

  it("is absent without a target and on the provider picker", () => {
    render(
      <SessionConfigControls
        configOptions={[MODEL]}
        pendingConfigOption={null}
        onSetConfigOption={vi.fn()}
        onSetProvider={vi.fn()}
        modelDefault={target}
      />,
    );
    fireEvent.click(byId("aoe-provider")!);
    expect(screen.queryByRole("menuitemcheckbox")).toBeNull();
    cleanup();
    mount([MODEL]);
    fireEvent.click(byId("model")!);
    expect(screen.queryByRole("menuitemcheckbox")).toBeNull();
    expect(defaults.loadAgentModelDefault).not.toHaveBeenCalled();
  });
});
