// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { TerminalSettings } from "../TerminalSettings";

vi.mock("../../lib/fontDetect", () => ({
  detectInstalledFonts: () => ["JetBrains Mono", "MesloLGS NF"],
}));

const KEY = "aoe-web-settings";

function readStored(): Record<string, unknown> {
  const raw = window.localStorage.getItem(KEY);
  return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
}

function seed(settings: Record<string, unknown>) {
  window.localStorage.setItem(KEY, JSON.stringify(settings));
}

beforeEach(() => {
  window.localStorage.clear();
});

describe("TerminalSettings localStorage contract", () => {
  it("labels font-size controls as applying to web tmux sessions", () => {
    const { getByText } = render(<TerminalSettings />);

    expect(getByText(/web terminal sessions on mobile devices, including tmux-backed sessions/i)).toBeTruthy();
    expect(getByText(/web terminal sessions on desktop, including tmux-backed sessions/i)).toBeTruthy();
  });

  it.each([
    ["mobile font slider", "input[type=range]", 0, "10", "mobileFontSize", 10],
    ["mobile font select", "select", 0, "16", "mobileFontSize", 16],
    ["desktop font slider", "input[type=range]", 1, "18", "desktopFontSize", 18],
    ["desktop font select", "select", 1, "20", "desktopFontSize", 20],
  ] as [string, string, number, string, string, number][])("%s writes %s", (_n, selector, i, value, key, expected) => {
    const { container } = render(<TerminalSettings />);
    fireEvent.change(container.querySelectorAll(selector)[i]!, { target: { value } });
    expect(readStored()[key]).toBe(expected);
  });

  it("font family input writes terminalFontFamily", () => {
    const { container } = render(<TerminalSettings />);
    const input = container.querySelector("#terminal-font-family") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "MesloLGS NF" } });
    expect(readStored().terminalFontFamily).toBe("MesloLGS NF");
  });

  it("lists detected fonts as datalist suggestions", () => {
    const { container } = render(<TerminalSettings />);
    const options = Array.from(container.querySelectorAll("#terminal-font-options option")).map(
      (o) => (o as HTMLOptionElement).value,
    );
    expect(options).toEqual(["JetBrains Mono", "MesloLGS NF"]);
  });

  it("reflects a stored terminalFontFamily on mount", () => {
    seed({ terminalFontFamily: "Fira Code" });
    const { container } = render(<TerminalSettings />);
    const input = container.querySelector("#terminal-font-family") as HTMLInputElement;
    expect(input.value).toBe("Fira Code");
  });

  it("autoOpenKeyboard checkbox writes the boolean flag", () => {
    const { container } = render(<TerminalSettings />);
    const checkbox = container.querySelectorAll("input[type=checkbox]")[0] as HTMLInputElement;
    fireEvent.click(checkbox);
    expect(readStored().autoOpenKeyboard).toBe(false);
  });

  it("sidebar side select writes sidebarSide into aoe-web-settings", () => {
    const { container } = render(<TerminalSettings />);
    const select = container.querySelector("#sidebar-side") as HTMLSelectElement;
    expect(select.value).toBe("left");
    fireEvent.change(select, { target: { value: "right" } });
    expect(readStored().sidebarSide).toBe("right");
  });

  it("reflects a stored sidebarSide on mount", () => {
    seed({ sidebarSide: "right" });
    const { container } = render(<TerminalSettings />);
    const select = container.querySelector("#sidebar-side") as HTMLSelectElement;
    expect(select.value).toBe("right");
  });

  it("persistent terminals checkbox writes the beta flag", () => {
    const { container } = render(<TerminalSettings />);
    const checkbox = container.querySelectorAll("input[type=checkbox]")[1] as HTMLInputElement;
    fireEvent.click(checkbox);
    expect(readStored().persistentTerminals).toBe(true);
  });

  it("persistent terminal limit input writes a clamped number", () => {
    seed({ persistentTerminals: true });
    const { container } = render(<TerminalSettings />);
    const input = container.querySelector("input[type=number]") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "50" } });
    expect(readStored().maxPersistentTerminals).toBe(50);

    fireEvent.change(input, { target: { value: "99" } });
    expect(readStored().maxPersistentTerminals).toBe(50);
  });

  it("preserves unrelated keys when persisting an update", () => {
    seed({
      mobileFontSize: 8,
      desktopFontSize: 14,
      autoOpenKeyboard: true,
      persistentTerminals: false,
      maxPersistentTerminals: 5,
      diffViewMode: "tree",
      collapsedDiffDirs: ["a/b"],
    });
    const { container } = render(<TerminalSettings />);
    const slider = container.querySelectorAll("input[type=range]")[0] as HTMLInputElement;
    fireEvent.change(slider, { target: { value: "12" } });
    const stored = readStored();
    expect(stored).toMatchObject({
      mobileFontSize: 12,
      desktopFontSize: 14,
      autoOpenKeyboard: true,
      persistentTerminals: false,
      maxPersistentTerminals: 5,
      diffViewMode: "tree",
      collapsedDiffDirs: ["a/b"],
    });
  });

  it("reflects the stored value on initial mount", () => {
    seed({
      mobileFontSize: 22,
      desktopFontSize: 16,
      autoOpenKeyboard: false,
      persistentTerminals: true,
      maxPersistentTerminals: 42,
    });
    const { container } = render(<TerminalSettings />);
    const mobileSelect = container.querySelectorAll("select")[0] as HTMLSelectElement;
    const desktopSelect = container.querySelectorAll("select")[1] as HTMLSelectElement;
    const checkboxes = container.querySelectorAll("input[type=checkbox]");
    const checkbox = checkboxes[0] as HTMLInputElement;
    const persistentCheckbox = checkboxes[1] as HTMLInputElement;
    const persistentLimit = container.querySelector("input[type=number]") as HTMLInputElement;
    expect(mobileSelect.value).toBe("22");
    expect(desktopSelect.value).toBe("16");
    expect(checkbox.checked).toBe(false);
    expect(persistentCheckbox.checked).toBe(true);
    expect(persistentLimit.value).toBe("42");
  });

  it("normalizes malformed persistent terminal settings on read", () => {
    seed({
      persistentTerminals: "yes",
      maxPersistentTerminals: 1000,
    });
    const { container } = render(<TerminalSettings />);
    const checkboxes = container.querySelectorAll("input[type=checkbox]");
    const persistentCheckbox = checkboxes[1] as HTMLInputElement;
    const persistentLimit = container.querySelector("input[type=number]") as HTMLInputElement | null;

    expect(persistentCheckbox.checked).toBe(false);
    expect(persistentLimit).toBeNull();
  });

  it("clamps a persisted terminal keep-alive limit on read", () => {
    seed({
      persistentTerminals: true,
      maxPersistentTerminals: 1000,
    });
    const { container } = render(<TerminalSettings />);
    const persistentLimit = container.querySelector("input[type=number]") as HTMLInputElement;

    expect(persistentLimit.value).toBe("50");
  });
});
