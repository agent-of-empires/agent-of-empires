// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { TerminalSettings } from "../TerminalSettings";

vi.mock("../../lib/fontDetect", () => ({
  detectInstalledFonts: () => ["Menlo", "MesloLGS NF"],
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
    ["font family select", "#terminal-font-family", 0, "JetBrains Mono", "terminalFontFamily", "JetBrains Mono"],
    ["sidebar side select", "#sidebar-side", 0, "right", "sidebarSide", "right"],
  ] as [string, string, number, string, string, unknown][])("%s writes %s", (_n, selector, i, value, key, expected) => {
    const { container } = render(<TerminalSettings />);
    fireEvent.change(container.querySelectorAll(selector)[i]!, { target: { value } });
    expect(readStored()[key]).toBe(expected);
  });

  it("checkboxes write the keyboard, persistent-terminal and scrollbar flags", () => {
    const { container } = render(<TerminalSettings />);
    const checkboxes = container.querySelectorAll("input[type=checkbox]");
    fireEvent.click(checkboxes[0]!);
    expect(readStored().autoOpenKeyboard).toBe(false);
    fireEvent.click(checkboxes[1]!);
    expect(readStored().persistentTerminals).toBe(true);
    fireEvent.click(checkboxes[2]!);
    expect(readStored().showForwardScrollbar).toBe(true);
  });

  it("offers bundled fonts, then only the system fonts detected here, each in its own font", () => {
    const { container } = render(<TerminalSettings />);
    const groups = Array.from(container.querySelectorAll("#terminal-font-family optgroup")).map((g) => [
      g.getAttribute("label"),
      Array.from(g.querySelectorAll("option")).map((o) => o.textContent),
    ]);
    expect(groups).toEqual([
      ["Bundled", ["Geist Mono (default)", "JetBrains Mono"]],
      ["Installed on this device", ["Menlo", "MesloLGS NF"]],
    ]);
    const menlo = container.querySelector('#terminal-font-family option[value="Menlo"]') as HTMLOptionElement;
    expect(menlo.style.fontFamily).toContain("Menlo");
    expect(container.querySelector("[data-testid=terminal-font-missing]")).toBeNull();
  });

  it("keeps a synced font this device lacks selected and says the default renders instead", () => {
    seed({ terminalFontFamily: "Fira Code" });
    const { container, getByTestId } = render(<TerminalSettings />);
    expect((container.querySelector("#terminal-font-family") as HTMLSelectElement).value).toBe("Fira Code");
    expect(getByTestId("terminal-font-missing").textContent).toBe(
      "Fira Code isn't installed on this device, so terminals here use Geist Mono.",
    );
    fireEvent.change(container.querySelector("#terminal-font-family")!, { target: { value: "" } });
    expect(readStored().terminalFontFamily).toBe("");
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
      terminalFontFamily: "menlo",
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
    expect((container.querySelector("#terminal-font-family") as HTMLSelectElement).value).toBe("Menlo");
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
