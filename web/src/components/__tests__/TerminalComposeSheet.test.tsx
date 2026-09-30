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
  ])("%s pastes the draft with submit=%s, then clears it and refocuses the terminal", (action, submit) => {
    const key = `deliver-${action}`;
    const first = renderSheet(key);
    expect(document.activeElement).toBe(first.text);
    expect(screen.getByRole("button", { name: action })).toHaveProperty("disabled", true);

    first.type("fix the\nbug");
    fireEvent.click(screen.getByRole("button", { name: action }));
    expect(first.onSubmit.mock.calls).toEqual([["fix the\nbug", submit]]);
    expect(first.onClose).toHaveBeenCalledWith(true);
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

  it("sends on Ctrl or Cmd+Enter and leaves a plain Enter as a newline", () => {
    const sheet = renderSheet("chord");
    sheet.type("go");
    expect(fireEvent.keyDown(sheet.text, { key: "Enter" })).toBe(true);
    expect(sheet.onSubmit).not.toHaveBeenCalled();
    fireEvent.keyDown(sheet.text, { key: "Enter", metaKey: true });
    expect(sheet.onSubmit).toHaveBeenCalledWith("go", true);
  });
});
