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
  DefaultToolWidget,
  LoggingTargetsWidget,
  SmartRenameAgentWidget,
  SoundVolumeWidget,
  ThemeNameWidget,
} from "../customWidgets";

const fetchThemes = vi.fn(() => Promise.resolve(["dark", "light"]));
const dispatchThemePickerChanged = vi.fn();
const agent = (name: string, installed: boolean, oneshot_capable: boolean) => ({
  name,
  kind: "builtin" as const,
  binary: name,
  host_only: false,
  installed,
  install_hint: "",
  oneshot_capable,
  acp_capable: true,
  acp_installed: true,
  acp_args: [],
});
const fetchAgents = vi.fn(() =>
  Promise.resolve([
    agent("claude", true, true),
    agent("codex", true, true),
    agent("gemini", false, true), // not installed -> filtered out
    agent("cursor", true, false), // no one-shot -> filtered out
  ]),
);

vi.mock("../../../lib/api", () => ({
  fetchThemes: () => fetchThemes(),
  fetchAgents: () => fetchAgents(),
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
  const el = Array.from(document.querySelectorAll("label")).find((l) => l.textContent === label);
  return el?.parentElement?.querySelector("select") as HTMLSelectElement;
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe("SoundVolumeWidget", () => {
  it("renders a 0.1-1.5 float slider and saves the number", () => {
    const save = vi.fn(() => Promise.resolve(true));
    const { container } = render(
      <SoundVolumeWidget descriptor={descriptor({ field: "volume", label: "Volume" })} value={1.0} save={save} />,
    );
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
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
    render(<ThemeNameWidget descriptor={descriptor({ field: "name", label: "Theme" })} value="dark" save={save} />);
    await waitFor(() => expect(screen.getByText("light")).toBeTruthy());

    fireEvent.change(selectByLabel("Theme"), { target: { value: "light" } });
    expect(save).toHaveBeenCalledWith("light");
    await waitFor(() => expect(dispatchThemePickerChanged).toHaveBeenCalledWith("light"));
  });

  it("does not repaint when the save fails", async () => {
    const save = vi.fn(() => Promise.resolve(false));
    render(<ThemeNameWidget descriptor={descriptor({ field: "name", label: "Theme" })} value="dark" save={save} />);
    await waitFor(() => expect(screen.getByText("light")).toBeTruthy());

    fireEvent.change(selectByLabel("Theme"), { target: { value: "light" } });
    await Promise.resolve();
    expect(dispatchThemePickerChanged).not.toHaveBeenCalled();
  });
});

// AcpDefaultsWidget moved to its own file and is covered by
// AcpDefaultsWidget.test.tsx (#2631).

describe("SmartRenameAgentWidget", () => {
  it("lists only installed one-shot-capable agents plus Same as session, and saves the name", async () => {
    const save = vi.fn(() => Promise.resolve(true));
    render(
      <SmartRenameAgentWidget
        descriptor={descriptor({ field: "smart_rename_agent", label: "Smart-rename agent" })}
        value=""
        save={save}
      />,
    );
    await waitFor(() => expect(screen.getByText("codex")).toBeTruthy());
    const select = selectByLabel("Smart-rename agent");
    const optionValues = Array.from(select.options).map((o) => o.value);
    // "Same as session" (empty) + installed one-shot agents only.
    expect(optionValues).toEqual(["", "claude", "codex"]);

    fireEvent.change(select, { target: { value: "codex" } });
    expect(save).toHaveBeenCalledWith("codex");

    // Empty selection persists as "" (use session agent), not null.
    fireEvent.change(selectByLabel("Smart-rename agent"), { target: { value: "" } });
    expect(save).toHaveBeenCalledWith("");
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
    const input = container.querySelector('input[type="text"]') as HTMLInputElement;
    // TextField commits on blur, not on each keystroke.
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.blur(input);
    expect(save).toHaveBeenCalledWith(null);
  });
});
