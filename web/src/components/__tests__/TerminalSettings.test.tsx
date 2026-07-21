// @vitest-environment jsdom
//
// Contract test for the TerminalSettings panel. Unlike the panels under
// settings/, this one persists through useWebSettings + localStorage
// (key `aoe-web-settings`) rather than PATCH /api/settings. The contract
// here is the JSON shape written to that key. Part of #1217.

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

beforeEach(() => {
  window.localStorage.clear();
});

describe("TerminalSettings localStorage contract", () => {
  it("labels font-size controls as applying to web tmux sessions", () => {
    const { getByText } = render(<TerminalSettings />);

    expect(getByText(/web terminal sessions on mobile devices, including tmux-backed sessions/i)).toBeTruthy();
    expect(getByText(/web terminal sessions on desktop, including tmux-backed sessions/i)).toBeTruthy();
  });

  it("mobile font slider writes mobileFontSize into aoe-web-settings", () => {
    const { container } = render(<TerminalSettings />);
    const slider = container.querySelectorAll("input[type=range]")[0] as HTMLInputElement;
    fireEvent.change(slider, { target: { value: "10" } });
    expect(readStored().mobileFontSize).toBe(10);
  });

  it("mobile font select writes mobileFontSize", () => {
    const { container } = render(<TerminalSettings />);
    const select = container.querySelectorAll("select")[0] as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "16" } });
    expect(readStored().mobileFontSize).toBe(16);
  });

  it("desktop font slider writes desktopFontSize", () => {
    const { container } = render(<TerminalSettings />);
    const slider = container.querySelectorAll("input[type=range]")[1] as HTMLInputElement;
    fireEvent.change(slider, { target: { value: "18" } });
    expect(readStored().desktopFontSize).toBe(18);
  });

  it("desktop font select writes desktopFontSize", () => {
    const { container } = render(<TerminalSettings />);
    const select = container.querySelectorAll("select")[1] as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "20" } });
    expect(readStored().desktopFontSize).toBe(20);
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
    window.localStorage.setItem(KEY, JSON.stringify({ terminalFontFamily: "Fira Code" }));
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
    window.localStorage.setItem(KEY, JSON.stringify({ sidebarSide: "right" }));
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
    window.localStorage.setItem(KEY, JSON.stringify({ persistentTerminals: true }));
    const { container } = render(<TerminalSettings />);
    const input = container.querySelector("input[type=number]") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "50" } });
    expect(readStored().maxPersistentTerminals).toBe(50);

    fireEvent.change(input, { target: { value: "99" } });
    expect(readStored().maxPersistentTerminals).toBe(50);
  });

  it("preserves unrelated keys when persisting an update", () => {
    window.localStorage.setItem(
      KEY,
      JSON.stringify({
        mobileFontSize: 8,
        desktopFontSize: 14,
        autoOpenKeyboard: true,
        persistentTerminals: false,
        maxPersistentTerminals: 5,
        diffViewMode: "tree",
        collapsedDiffDirs: ["a/b"],
      }),
    );
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
    window.localStorage.setItem(
      KEY,
      JSON.stringify({
        mobileFontSize: 22,
        desktopFontSize: 16,
        autoOpenKeyboard: false,
        persistentTerminals: true,
        maxPersistentTerminals: 42,
      }),
    );
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
    window.localStorage.setItem(
      KEY,
      JSON.stringify({
        persistentTerminals: "yes",
        maxPersistentTerminals: 1000,
      }),
    );
    const { container } = render(<TerminalSettings />);
    const checkboxes = container.querySelectorAll("input[type=checkbox]");
    const persistentCheckbox = checkboxes[1] as HTMLInputElement;
    const persistentLimit = container.querySelector("input[type=number]") as HTMLInputElement | null;

    expect(persistentCheckbox.checked).toBe(false);
    expect(persistentLimit).toBeNull();
  });

  it("clamps a persisted terminal keep-alive limit on read", () => {
    window.localStorage.setItem(
      KEY,
      JSON.stringify({
        persistentTerminals: true,
        maxPersistentTerminals: 1000,
      }),
    );
    const { container } = render(<TerminalSettings />);
    const persistentLimit = container.querySelector("input[type=number]") as HTMLInputElement;

    expect(persistentLimit.value).toBe("50");
  });
});
