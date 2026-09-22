// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { SettingsFieldDescriptor } from "../../../lib/types";
import {
  DefaultToolWidget,
  LoggingTargetsWidget,
  SmartRenameAgentWidget,
  SmartRenameModelWidget,
  SoundVolumeWidget,
  ThemeNameWidget,
  type CustomWidgetProps,
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
    agent("gemini", false, true),
    agent("cursor", true, false),
  ]),
);

vi.mock("../../../lib/api", () => ({
  fetchThemes: () => fetchThemes(),
  fetchAgents: () => fetchAgents(),
}));
vi.mock("../../../hooks/useResolvedTheme", () => ({
  dispatchThemePickerChanged: (t?: string) => dispatchThemePickerChanged(t),
}));

beforeEach(() => {
  vi.clearAllMocks();
});

function mount(Widget: (p: CustomWidgetProps) => React.ReactElement, label: string, value: unknown, ok = true) {
  const save = vi.fn(() => Promise.resolve(ok));
  const descriptor = {
    section: "x",
    field: "f",
    category: "X",
    label,
    description: "",
    widget: { kind: "custom", id: "f" },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  } as SettingsFieldDescriptor;
  const utils = render(<Widget descriptor={descriptor} value={value} save={save} />);
  const rerender = (next: unknown) => utils.rerender(<Widget descriptor={descriptor} value={next} save={save} />);
  return { save, rerender, container: utils.container };
}

const control = (label: string, tag: "select" | "input") =>
  Array.from(document.querySelectorAll("label"))
    .find((l) => l.textContent === label)
    ?.parentElement?.querySelector(tag) as HTMLSelectElement & HTMLInputElement;

function commitText(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

it("SoundVolumeWidget saves a 0.1-1.5 float", () => {
  const { save, container } = mount(SoundVolumeWidget, "Volume", 1.0);
  const slider = container.querySelector<HTMLInputElement>('input[type="range"]')!;
  expect([slider.min, slider.max]).toEqual(["0.1", "1.5"]);
  fireEvent.change(slider, { target: { value: "0.5" } });
  expect(save).toHaveBeenCalledWith(0.5);
});

it("LoggingTargetsWidget sets an override and removes it on (default)", () => {
  const { save, rerender } = mount(LoggingTargetsWidget, "Targets", {});
  fireEvent.change(control("acp.protocol", "select"), { target: { value: "debug" } });
  expect(save).toHaveBeenCalledWith({ "acp.protocol": "debug" });
  rerender({ "acp.protocol": "debug" });
  fireEvent.change(control("acp.protocol", "select"), { target: { value: "" } });
  expect(save).toHaveBeenCalledWith({});
});

describe("ThemeNameWidget", () => {
  it.each([
    [true, 1],
    [false, 0],
  ])("repaints only when the save succeeds (%s)", async (ok, repaints) => {
    const { save } = mount(ThemeNameWidget, "Theme", "dark", ok);
    await screen.findByText("light");
    fireEvent.change(control("Theme", "select"), { target: { value: "light" } });
    expect(save).toHaveBeenCalledWith("light");
    await Promise.resolve();
    await waitFor(() => expect(dispatchThemePickerChanged).toHaveBeenCalledTimes(repaints));
  });
});

it("SmartRenameAgentWidget lists installed one-shot agents plus Same as session", async () => {
  const { save } = mount(SmartRenameAgentWidget, "Agent", "");
  await screen.findByText("codex");
  const select = control("Agent", "select");
  expect(Array.from(select.options).map((o) => o.value)).toEqual(["", "claude", "codex"]);
  fireEvent.change(select, { target: { value: "codex" } });
  expect(save).toHaveBeenCalledWith("codex");
  fireEvent.change(select, { target: { value: "" } });
  expect(save).toHaveBeenCalledWith("");
});

describe("SmartRenameModelWidget", () => {
  it("renders a row per installed one-shot agent and sets an override", async () => {
    const { save } = mount(SmartRenameModelWidget, "Model", {});
    await waitFor(() => expect(control("codex", "input")).toBeTruthy());
    expect(control("gemini", "input")).toBeFalsy();
    expect(control("cursor", "input")).toBeFalsy();
    commitText(control("claude", "input"), "haiku");
    expect(save).toHaveBeenCalledWith({ claude: "haiku" });
  });

  it("clearing a row removes only that key", async () => {
    const { save } = mount(SmartRenameModelWidget, "Model", { claude: "haiku", codex: "gpt-5" });
    await waitFor(() => expect(control("claude", "input")).toBeTruthy());
    commitText(control("claude", "input"), "");
    expect(save).toHaveBeenCalledWith({ codex: "gpt-5" });
  });
});

it("DefaultToolWidget clears to null when emptied", () => {
  const { save, container } = mount(DefaultToolWidget, "Default agent", "claude");
  commitText(container.querySelector<HTMLInputElement>('input[type="text"]')!, "");
  expect(save).toHaveBeenCalledWith(null);
});
