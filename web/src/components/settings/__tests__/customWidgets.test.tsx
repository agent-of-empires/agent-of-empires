// @vitest-environment jsdom
//
// Contract tests for the settings custom-widget registry (#1792). Each widget
// renders one schema field whose `widget.kind === "custom"` and owns its own
// bespoke encoding / side-effect; these pin (a) the value <-> save shape and
// (b) the theme repaint firing only after a successful save.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { SettingsFieldDescriptor } from "../../../lib/types";
import {
  AcpDefaultsWidget,
  DefaultToolWidget,
  LoggingTargetsWidget,
  SoundModeWidget,
  SoundVolumeWidget,
  ThemeNameWidget,
} from "../customWidgets";

const fetchThemes = vi.fn(() => Promise.resolve(["dark", "light"]));
const dispatchThemePickerChanged = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchThemes: () => fetchThemes(),
}));
vi.mock("../../../hooks/useResolvedTheme", () => ({
  dispatchThemePickerChanged: (t?: string) => dispatchThemePickerChanged(t),
}));

function descriptor(
  over: Partial<SettingsFieldDescriptor> & { field: string; label: string },
): SettingsFieldDescriptor {
  return {
    section: "x",
    category: "X",
    description: "",
    widget: { kind: "custom", id: over.field },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
    ...over,
  };
}

function selectByLabel(label: string): HTMLSelectElement {
  const el = Array.from(document.querySelectorAll("label")).find(
    (l) => l.textContent === label,
  );
  return el?.parentElement?.querySelector("select") as HTMLSelectElement;
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe("SoundModeWidget", () => {
  it("maps the random/specific enum onto the select", () => {
    const save = vi.fn(() => Promise.resolve(true));
    const { rerender } = render(
      <SoundModeWidget
        descriptor={descriptor({ field: "mode", label: "Mode" })}
        value="random"
        save={save}
      />,
    );
    const select = selectByLabel("Mode");
    expect(select.value).toBe("random");

    fireEvent.change(select, { target: { value: "specific" } });
    expect(save).toHaveBeenCalledWith({ specific: "" });

    // An object value reads back as "specific".
    rerender(
      <SoundModeWidget
        descriptor={descriptor({ field: "mode", label: "Mode" })}
        value={{ specific: "chime.wav" }}
        save={save}
      />,
    );
    expect(selectByLabel("Mode").value).toBe("specific");

    fireEvent.change(selectByLabel("Mode"), { target: { value: "random" } });
    expect(save).toHaveBeenCalledWith("random");
  });
});

describe("SoundVolumeWidget", () => {
  it("renders a 0.1-1.5 float slider and saves the number", () => {
    const save = vi.fn(() => Promise.resolve(true));
    const { container } = render(
      <SoundVolumeWidget
        descriptor={descriptor({ field: "volume", label: "Volume" })}
        value={1.0}
        save={save}
      />,
    );
    const slider = container.querySelector(
      'input[type="range"]',
    ) as HTMLInputElement;
    expect(slider.min).toBe("0.1");
    expect(slider.max).toBe("1.5");
    fireEvent.change(slider, { target: { value: "0.5" } });
    expect(save).toHaveBeenCalledWith(0.5);
  });
});

describe("LoggingTargetsWidget", () => {
  it("sets a per-target override and removes it on (default)", () => {
    const save = vi.fn(() => Promise.resolve(true));
    const { rerender } = render(
      <LoggingTargetsWidget
        descriptor={descriptor({
          field: "targets",
          label: "Per-target overrides",
        })}
        value={{}}
        save={save}
      />,
    );
    fireEvent.change(selectByLabel("acp.protocol"), {
      target: { value: "debug" },
    });
    expect(save).toHaveBeenCalledWith({ "acp.protocol": "debug" });

    rerender(
      <LoggingTargetsWidget
        descriptor={descriptor({
          field: "targets",
          label: "Per-target overrides",
        })}
        value={{ "acp.protocol": "debug" }}
        save={save}
      />,
    );
    fireEvent.change(selectByLabel("acp.protocol"), { target: { value: "" } });
    expect(save).toHaveBeenCalledWith({});
  });
});

describe("ThemeNameWidget", () => {
  it("lists fetched themes and repaints only after a successful save", async () => {
    const save = vi.fn(() => Promise.resolve(true));
    render(
      <ThemeNameWidget
        descriptor={descriptor({ field: "name", label: "Theme" })}
        value="dark"
        save={save}
      />,
    );
    await waitFor(() => expect(screen.getByText("light")).toBeTruthy());

    fireEvent.change(selectByLabel("Theme"), { target: { value: "light" } });
    expect(save).toHaveBeenCalledWith("light");
    await waitFor(() =>
      expect(dispatchThemePickerChanged).toHaveBeenCalledWith("light"),
    );
  });

  it("does not repaint when the save fails", async () => {
    const save = vi.fn(() => Promise.resolve(false));
    render(
      <ThemeNameWidget
        descriptor={descriptor({ field: "name", label: "Theme" })}
        value="dark"
        save={save}
      />,
    );
    await waitFor(() => expect(screen.getByText("light")).toBeTruthy());

    fireEvent.change(selectByLabel("Theme"), { target: { value: "light" } });
    await Promise.resolve();
    expect(dispatchThemePickerChanged).not.toHaveBeenCalled();
  });
});

describe("AcpDefaultsWidget", () => {
  it("saves a parsed object and ignores invalid JSON", () => {
    const save = vi.fn(() => Promise.resolve(true));
    const { container } = render(
      <AcpDefaultsWidget
        descriptor={descriptor({
          field: "acp_defaults",
          label: "Structured View Defaults",
        })}
        value={{ opencode: { model: "x" } }}
        save={save}
      />,
    );
    const ta = container.querySelector("textarea") as HTMLTextAreaElement;
    // Renders the current map as pretty JSON.
    expect(ta.value).toContain("opencode");

    // Invalid JSON: not saved (field keeps its last valid value).
    fireEvent.focus(ta);
    fireEvent.change(ta, { target: { value: "{not json" } });
    fireEvent.blur(ta);
    expect(save).not.toHaveBeenCalled();

    // Valid object: saved as a parsed object.
    fireEvent.focus(ta);
    fireEvent.change(ta, {
      target: { value: '{"claude":{"effort":"high"}}' },
    });
    fireEvent.blur(ta);
    expect(save).toHaveBeenCalledWith({ claude: { effort: "high" } });
  });
});

describe("DefaultToolWidget", () => {
  it("clears the value to null when emptied", () => {
    const save = vi.fn(() => Promise.resolve(true));
    const { container } = render(
      <DefaultToolWidget
        descriptor={descriptor({
          field: "default_tool",
          label: "Default agent",
        })}
        value="claude"
        save={save}
      />,
    );
    const input = container.querySelector(
      'input[type="text"]',
    ) as HTMLInputElement;
    // TextField commits on blur, not on each keystroke.
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.blur(input);
    expect(save).toHaveBeenCalledWith(null);
  });
});
