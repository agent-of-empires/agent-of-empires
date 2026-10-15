// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { TerminalComposeSheet } from "../TerminalComposeSheet";

afterEach(cleanup);

function renderSheet(draftKey: string, delivered = true) {
  const onSubmit = vi.fn<(text: string, submit: boolean) => boolean>(() => delivered);
  const onClose = vi.fn<(refocusTerminal: boolean) => void>();
  const utils = render(
    <TerminalComposeSheet draftKey={draftKey} bottomInset={0} onSubmit={onSubmit} onClose={onClose} />,
  );
  const text = screen.getByLabelText<HTMLTextAreaElement>("Message");
  const type = (value: string) => fireEvent.input(text, { target: { value } });
  return { ...utils, onSubmit, onClose, text, type };
}

describe("TerminalComposeSheet", () => {
  it.each([
    ["Send", true],
    ["Insert", false],
  ])("%s pastes the draft with submit=%s, then clears it", (action, submit) => {
    const key = `deliver-${action}`;
    const first = renderSheet(key);
    expect(document.activeElement).toBe(first.text);
    expect(screen.getByRole("button", { name: action })).toHaveProperty("disabled", true);

    first.type("fix the\nbug");
    fireEvent.click(screen.getByRole("button", { name: action }));
    expect(first.onSubmit.mock.calls).toEqual([["fix the\nbug", submit]]);
    // Only Insert keeps the keyboard up for the terminal.
    expect(first.onClose).toHaveBeenCalledWith(!submit);
    expect(document.activeElement === first.text).toBe(!submit);
    first.unmount();

    expect(renderSheet(key).text.value).toBe("");
  });

  it("keeps the draft per terminal across close, backdrop, and Escape", () => {
    const sheet = renderSheet("keep-a");
    sheet.type("half written");
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    fireEvent.click(document.querySelector("[data-compose-backdrop]")!);
    fireEvent.keyDown(sheet.text, { key: "Escape" });
    expect(sheet.onClose.mock.calls).toEqual([[false], [false], [false]]);
    expect(sheet.onSubmit).not.toHaveBeenCalled();
    sheet.unmount();

    expect(renderSheet("keep-a").text.value).toBe("half written");
    cleanup();
    expect(renderSheet("keep-b").text.value).toBe("");
  });

  it("stays open with the draft when the pane cannot take the paste", () => {
    const sheet = renderSheet("undelivered", false);
    sheet.type("still here");
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(sheet.onSubmit).toHaveBeenCalledWith("still here", true);
    expect(sheet.onClose).not.toHaveBeenCalled();
    sheet.unmount();
    expect(renderSheet("undelivered").text.value).toBe("still here");
  });

  it("sends on Ctrl or Cmd+Enter, keeping the terminal focused, and leaves a plain Enter as a newline", () => {
    const sheet = renderSheet("chord");
    sheet.type("go");
    expect(fireEvent.keyDown(sheet.text, { key: "Enter" })).toBe(true);
    expect(sheet.onSubmit).not.toHaveBeenCalled();
    fireEvent.keyDown(sheet.text, { key: "Enter", metaKey: true });
    expect(sheet.onSubmit).toHaveBeenCalledWith("go", true);
    expect(sheet.onClose).toHaveBeenCalledWith(true);
  });

  it.each([
    ["empty", "", "Close"],
    ["nonempty", "draft", "Send"],
  ])("wraps Tab and Shift+Tab inside the sheet with a %s draft", (_name, draft, lastName) => {
    const sheet = renderSheet(`wrap-${_name}`);
    if (draft) sheet.type(draft);
    const last = screen.getByRole("button", { name: lastName });

    last.focus();
    expect(fireEvent.keyDown(last, { key: "Tab" })).toBe(false);
    expect(document.activeElement).toBe(sheet.text);

    expect(fireEvent.keyDown(sheet.text, { key: "Tab", shiftKey: true })).toBe(false);
    expect(document.activeElement).toBe(last);

    // Mid-sheet Tab is left to the browser.
    const close = screen.getByRole("button", { name: "Close" });
    close.focus();
    expect(fireEvent.keyDown(close, { key: "Tab", shiftKey: true })).toBe(true);
  });

  it("closes on Escape from every control and keeps the draft", () => {
    const sheet = renderSheet("escape-all");
    sheet.type("keep me");
    for (const name of ["Close", "Insert", "Send"]) {
      fireEvent.keyDown(screen.getByRole("button", { name }), { key: "Escape" });
    }
    expect(sheet.onClose.mock.calls).toEqual([[false], [false], [false]]);
    sheet.unmount();
    expect(renderSheet("escape-all").text.value).toBe("keep me");
  });
});
