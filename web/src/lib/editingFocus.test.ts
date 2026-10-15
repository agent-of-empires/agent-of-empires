// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { installEditingFocus, raisesKeyboard } from "./editingFocus";

describe("raisesKeyboard", () => {
  it.each([
    ["<textarea></textarea>", true],
    ["<input>", true],
    ['<input type="search">', true],
    ['<input type="checkbox">', false],
    ['<button type="button"></button>', false],
    ['<div contenteditable="true"></div>', true],
    ["<div></div>", false],
  ])("%s -> %s", (html, expected) => {
    document.body.innerHTML = html;
    const el = document.body.firstElementChild!;
    // jsdom does not compute isContentEditable.
    if (el.getAttribute("contenteditable") === "true") Object.defineProperty(el, "isContentEditable", { value: true });
    expect(raisesKeyboard(el)).toBe(expected);
  });
});

describe("installEditingFocus", () => {
  let uninstall: () => void = () => {};
  afterEach(() => {
    uninstall();
    document.body.innerHTML = "";
    document.documentElement.removeAttribute("data-editing");
  });

  it("leaves every other platform untouched", () => {
    document.body.innerHTML = "<input>";
    uninstall = installEditingFocus(document, false);
    document.querySelector("input")!.focus();
    expect(document.documentElement.hasAttribute("data-ios-standalone")).toBe(false);
    expect(document.documentElement.hasAttribute("data-editing")).toBe(false);
  });

  it("marks <html> while a text field has focus, across a focus move and after blur", async () => {
    document.body.innerHTML = '<input id="a"><textarea id="b"></textarea><button id="c"></button>';
    uninstall = installEditingFocus(document, true);
    const html = document.documentElement;
    expect(html.hasAttribute("data-ios-standalone")).toBe(true);
    expect(html.hasAttribute("data-editing")).toBe(false);

    (document.getElementById("a") as HTMLInputElement).focus();
    expect(html.hasAttribute("data-editing")).toBe(true);
    (document.getElementById("b") as HTMLTextAreaElement).focus();
    await Promise.resolve();
    expect(html.hasAttribute("data-editing")).toBe(true);
    (document.getElementById("c") as HTMLButtonElement).focus();
    await Promise.resolve();
    expect(html.hasAttribute("data-editing")).toBe(false);
  });
});
