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
  it.each([
    ["mobile font slider", "input[type=range]", 0, "10", "mobileFontSize", 10],
    ["mobile font select", "select", 0, "16", "mobileFontSize", 16],
    ["desktop font slider", "input[type=range]", 1, "18", "desktopFontSize", 18],
    ["desktop font select", "select", 1, "20", "desktopFontSize", 20],
    ["font family input", "#terminal-font-family", 0, "MesloLGS NF", "terminalFontFamily", "MesloLGS NF"],
    ["sidebar side select", "#sidebar-side", 0, "right", "sidebarSide", "right"],
  ] as [string, string, number, string, string, unknown][])("%s writes %s", (_n, selector, i, value, key, expected) => {
    const { container } = render(<TerminalSettings />);
    fireEvent.change(container.querySelectorAll(selector)[i]!, { target: { value } });
    expect(readStored()[key]).toBe(expected);
  });

  it("checkboxes write the keyboard and persistent-terminal flags", () => {
    const { container } = render(<TerminalSettings />);
    const checkboxes = container.querySelectorAll("input[type=checkbox]");
    fireEvent.click(checkboxes[0]!);
    expect(readStored().autoOpenKeyboard).toBe(false);
    fireEvent.click(checkboxes[1]!);
    expect(readStored().persistentTerminals).toBe(true);
  });

  it("lists detected fonts as datalist suggestions", () => {
    const { container } = render(<TerminalSettings />);
    const options = Array.from(container.querySelectorAll("#terminal-font-options option")).map(
      (o) => (o as HTMLOptionElement).value,
    );
    expect(options).toEqual(["JetBrains Mono", "MesloLGS NF"]);
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
    const seeded = {
      mobileFontSize: 8,
      desktopFontSize: 14,
      autoOpenKeyboard: true,
      persistentTerminals: false,
      maxPersistentTerminals: 5,
      diffViewMode: "tree",
      collapsedDiffDirs: ["a/b"],
    };
    seed(seeded);
    const { container } = render(<TerminalSettings />);
    fireEvent.change(container.querySelectorAll("input[type=range]")[0]!, { target: { value: "12" } });
    expect(readStored()).toMatchObject({ ...seeded, mobileFontSize: 12 });
  });

  it("reflects stored values on mount", () => {
    seed({
      mobileFontSize: 22,
      desktopFontSize: 16,
      autoOpenKeyboard: false,
      persistentTerminals: true,
      maxPersistentTerminals: 42,
      terminalFontFamily: "Fira Code",
      sidebarSide: "right",
    });
    const { container } = render(<TerminalSettings />);
    const selects = container.querySelectorAll("select");
    const checkboxes = container.querySelectorAll("input[type=checkbox]");
    expect((selects[0] as HTMLSelectElement).value).toBe("22");
    expect((selects[1] as HTMLSelectElement).value).toBe("16");
    expect((checkboxes[0] as HTMLInputElement).checked).toBe(false);
    expect((checkboxes[1] as HTMLInputElement).checked).toBe(true);
    expect((container.querySelector("input[type=number]") as HTMLInputElement).value).toBe("42");
    expect((container.querySelector("#terminal-font-family") as HTMLInputElement).value).toBe("Fira Code");
    expect((container.querySelector("#sidebar-side") as HTMLSelectElement).value).toBe("right");
  });

  it("normalizes malformed or out-of-range persistent terminal settings on read", () => {
    seed({ persistentTerminals: "yes", maxPersistentTerminals: 1000 });
    const first = render(<TerminalSettings />);
    expect((first.container.querySelectorAll("input[type=checkbox]")[1] as HTMLInputElement).checked).toBe(false);
    expect(first.container.querySelector("input[type=number]")).toBeNull();
    first.unmount();

    seed({ persistentTerminals: true, maxPersistentTerminals: 1000 });
    const { container } = render(<TerminalSettings />);
    expect((container.querySelector("input[type=number]") as HTMLInputElement).value).toBe("50");
  });
});
