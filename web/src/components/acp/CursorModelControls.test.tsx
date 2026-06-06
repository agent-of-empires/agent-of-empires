// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";

import { switchAcpAgent } from "../../lib/api";
import { CursorModelControls } from "./CursorModelControls";
import type { ConfigOptionDescriptor } from "../../lib/acpTypes";

vi.mock("../../lib/api", () => ({
  switchAcpAgent: vi.fn(),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function cursorModelOption(): ConfigOptionDescriptor {
  return {
    id: "cursor-model",
    name: "Model",
    category: "model",
    current_value: "composer-2.5-fast",
    options: [
      { value: "composer-2.5", name: "Composer 2.5" },
      { value: "composer-2.5-fast", name: "Composer 2.5 Fast" },
      { value: "gpt-5.3-codex-high", name: "Codex 5.3 High" },
      { value: "gpt-5.3-codex-high-fast", name: "Codex 5.3 High Fast" },
    ],
  };
}

function cursorBracketModelOption(): ConfigOptionDescriptor {
  return {
    id: "model",
    name: "Model",
    category: "model",
    current_value: "default[]",
    options: [
      { value: "default[]", name: "Auto" },
      { value: "composer-2.5[fast=true]", name: "composer-2.5" },
      {
        value: "gpt-5.3-codex[reasoning=medium,fast=false]",
        name: "gpt-5.3-codex",
      },
    ],
  };
}

function cursorParameterizedModelOption(
  currentValue = "composer-2.5",
): ConfigOptionDescriptor {
  return {
    id: "model",
    name: "Model",
    category: "model",
    current_value: currentValue,
    options: [
      { value: "default", name: "Auto" },
      { value: "composer-2.5", name: "Composer 2.5" },
      { value: "gpt-5.3-codex", name: "Codex 5.3" },
    ],
  };
}

function cursorFastOption(currentValue = "false"): ConfigOptionDescriptor {
  return {
    id: "fast",
    name: "Fast",
    category: "model_config",
    current_value: currentValue,
    options: [
      { value: "false", name: "Off" },
      { value: "true", name: "Fast" },
    ],
  };
}

describe("CursorModelControls", () => {
  it("renders nothing outside Cursor sessions", () => {
    const { container } = render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="codex"
        currentModel={null}
      />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("filters Cursor models and switches base model from the structured view toolbar", async () => {
    const setConfigOption = vi.fn();

    render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="cursor"
        currentModel="composer-2.5-fast"
        modelConfigOption={cursorModelOption()}
        onSetConfigOption={setConfigOption}
      />,
    );

    fireEvent.click(screen.getByTestId("cursor-model-trigger"));
    fireEvent.change(
      screen.getByRole("combobox", { name: "Search Cursor model" }),
      {
        target: { value: "codex high" },
      },
    );

    expect(
      screen.getByTestId("cursor-model-option-gpt-5.3-codex-high"),
    ).toBeTruthy();
    expect(screen.queryByTestId("cursor-model-option-composer-2.5")).toBeNull();

    fireEvent.click(
      screen.getByTestId("cursor-model-option-gpt-5.3-codex-high"),
    );

    await waitFor(() =>
      expect(setConfigOption).toHaveBeenCalledWith(
        "cursor-model",
        "gpt-5.3-codex-high",
      ),
    );
    expect(switchAcpAgent).not.toHaveBeenCalled();
  });

  it("renders for Cursor sessions even when the runtime agent name is cursor-agent", () => {
    render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="cursor-agent"
        sessionTool="cursor"
        currentModel={null}
      />,
    );

    expect(screen.getByTestId("cursor-model-trigger")).toBeTruthy();
  });

  it("keeps Fast mode separate from the model choice", async () => {
    const setConfigOption = vi.fn();

    render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="cursor"
        currentModel="composer-2.5-fast"
        modelConfigOption={cursorModelOption()}
        onSetConfigOption={setConfigOption}
      />,
    );

    const fast = screen.getByRole("switch", { name: "Cursor Fast mode" });
    expect(fast.getAttribute("aria-checked")).toBe("false");

    fireEvent.click(fast);

    await waitFor(() =>
      expect(setConfigOption).toHaveBeenCalledWith(
        "cursor-model",
        "composer-2.5-fast",
      ),
    );
    expect(switchAcpAgent).not.toHaveBeenCalled();
  });

  it("hides fast-only adapter models while Fast mode is off", () => {
    const setConfigOption = vi.fn();

    render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="cursor"
        currentModel={null}
        modelConfigOption={cursorBracketModelOption()}
        onSetConfigOption={setConfigOption}
      />,
    );

    expect(
      screen
        .getByRole("switch", { name: "Cursor Fast mode" })
        .getAttribute("aria-checked"),
    ).toBe("false");

    fireEvent.click(screen.getByTestId("cursor-model-trigger"));
    fireEvent.change(
      screen.getByRole("combobox", { name: "Search Cursor model" }),
      {
        target: { value: "composer" },
      },
    );

    expect(screen.queryByTestId("cursor-model-option-composer-2.5")).toBeNull();
    expect(setConfigOption).not.toHaveBeenCalled();
  });

  it("sends the adapter's exact fast=true value only after Fast mode is enabled", async () => {
    const setConfigOption = vi.fn();

    render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="cursor"
        currentModel={null}
        modelConfigOption={cursorBracketModelOption()}
        onSetConfigOption={setConfigOption}
      />,
    );

    fireEvent.click(screen.getByRole("switch", { name: "Cursor Fast mode" }));
    fireEvent.click(screen.getByTestId("cursor-model-trigger"));
    fireEvent.change(
      screen.getByRole("combobox", { name: "Search Cursor model" }),
      {
        target: { value: "composer" },
      },
    );
    fireEvent.click(screen.getByTestId("cursor-model-option-composer-2.5"));

    await waitFor(() =>
      expect(setConfigOption).toHaveBeenCalledWith(
        "model",
        "composer-2.5[fast=true]",
      ),
    );
  });

  it("uses plain model values when Cursor advertises parameterized model options", async () => {
    const setConfigOption = vi.fn();

    render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="cursor"
        currentModel={null}
        modelConfigOption={cursorParameterizedModelOption("default")}
        fastConfigOption={cursorFastOption("false")}
        onSetConfigOption={setConfigOption}
      />,
    );

    expect(
      screen
        .getByRole("switch", { name: "Cursor Fast mode" })
        .getAttribute("aria-checked"),
    ).toBe("false");

    fireEvent.click(screen.getByTestId("cursor-model-trigger"));
    fireEvent.change(
      screen.getByRole("combobox", { name: "Search Cursor model" }),
      {
        target: { value: "composer" },
      },
    );
    fireEvent.click(screen.getByTestId("cursor-model-option-composer-2.5"));

    await waitFor(() =>
      expect(setConfigOption).toHaveBeenCalledWith("model", "composer-2.5"),
    );
  });

  it("toggles Cursor parameterized Fast mode through the fast config option", async () => {
    const setConfigOption = vi.fn();

    render(
      <CursorModelControls
        sessionId="s1"
        currentAgent="cursor"
        currentModel={null}
        modelConfigOption={cursorParameterizedModelOption("composer-2.5")}
        fastConfigOption={cursorFastOption("false")}
        onSetConfigOption={setConfigOption}
      />,
    );

    const fast = screen.getByRole("switch", { name: "Cursor Fast mode" });
    expect(fast.getAttribute("aria-checked")).toBe("false");

    fireEvent.click(fast);

    await waitFor(() =>
      expect(setConfigOption).toHaveBeenCalledWith("fast", "true"),
    );
    expect(setConfigOption).not.toHaveBeenCalledWith(
      "model",
      expect.any(String),
    );
  });

  it("keeps the parameterized Fast switch optimistic until the adapter confirms", async () => {
    const setConfigOption = vi.fn().mockResolvedValue(undefined);
    const props = {
      sessionId: "s1",
      currentAgent: "cursor",
      currentModel: null,
      modelConfigOption: cursorParameterizedModelOption("composer-2.5"),
      onSetConfigOption: setConfigOption,
    };

    const { rerender } = render(
      <CursorModelControls
        {...props}
        fastConfigOption={cursorFastOption("false")}
      />,
    );

    const fast = screen.getByRole("switch", { name: "Cursor Fast mode" });
    fireEvent.click(fast);

    await waitFor(() =>
      expect(setConfigOption).toHaveBeenCalledWith("fast", "true"),
    );
    expect(fast.getAttribute("aria-checked")).toBe("true");

    rerender(
      <CursorModelControls
        {...props}
        fastConfigOption={cursorFastOption("false")}
      />,
    );
    expect(
      screen
        .getByRole("switch", { name: "Cursor Fast mode" })
        .getAttribute("aria-checked"),
    ).toBe("true");

    rerender(
      <CursorModelControls
        {...props}
        fastConfigOption={cursorFastOption("true")}
      />,
    );
    expect(
      screen
        .getByRole("switch", { name: "Cursor Fast mode" })
        .getAttribute("aria-checked"),
    ).toBe("true");
  });
});
