// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";

import {
  composerWrapperLayout,
  decideArrowRecall,
  decideBeforeInputAction,
  decideEnterAction,
  insertAtCaret,
  insertNewlineAtCaret,
  insertSlashCommand,
  IOS_ACCESSORY_BAR_PX,
} from "./composerInput";

const keys = { key: "Enter", shiftKey: false, ctrlKey: false, metaKey: false, altKey: false, isComposing: false };

describe("decideEnterAction", () => {
  it.each([
    // Only desktop mid-turn plain Enter takes the queue path; touch Enter is always a newline.
    [{}, false, true, "send"],
    [{}, false, false, "default"],
    [{}, true, true, "default"],
    [{}, true, false, "default"],
    [{ key: "a" }, false, true, "default"],
    [{ isComposing: true }, false, true, "default"],
    [{ shiftKey: true }, false, true, "default"],
    [{ ctrlKey: true }, false, true, "default"],
    [{ metaKey: true }, false, true, "default"],
  ])("%o mobile=%s turnActive=%s -> %s", (over, isMobile, turnActive, expected) => {
    expect(decideEnterAction({ ...keys, ...over }, { isMobile, turnActive })).toBe(expected);
  });
});

describe("decideArrowRecall", () => {
  const up = { ...keys, key: "ArrowUp" };
  const down = { ...keys, key: "ArrowDown" };
  it.each([
    [up, true, false, 2, "older"],
    [up, false, false, 2, "default"],
    [up, true, false, 0, "default"],
    [up, false, true, 2, "older"],
    [down, false, true, 2, "newer"],
    [down, true, false, 2, "default"],
    [{ ...up, key: "a" }, true, true, 2, "default"],
    ...[{ shiftKey: true }, { ctrlKey: true }, { metaKey: true }, { altKey: true }, { isComposing: true }].map(
      (mod) => [{ ...up, ...mod }, true, true, 2, "default"] as const,
    ),
  ])("%o caretAtStart=%s browsing=%s queueLen=%i -> %s", (event, caretAtStart, browsing, queueLen, expected) => {
    expect(decideArrowRecall(event, { caretAtStart, browsing, queueLen })).toBe(expected);
  });
});

describe("decideBeforeInputAction", () => {
  it.each([
    ["insertLineBreak", false, true, "newline"],
    ["insertParagraph", false, true, "newline"],
    ["insertText", false, true, "default"],
    ["deleteContentBackward", false, true, "default"],
    ["insertLineBreak", false, false, "default"],
    ["insertParagraph", false, false, "default"],
    ["insertLineBreak", true, true, "default"],
    ["insertParagraph", true, true, "default"],
  ])("%s composing=%s mobile=%s -> %s", (inputType, isComposing, isMobile, expected) => {
    expect(decideBeforeInputAction(inputType, isComposing, { isMobile })).toBe(expected);
  });
});

describe("composerWrapperLayout", () => {
  const BASE = ["border-t", "border-surface-800", "bg-surface-900", "px-4", "pt-3"];
  it.each([
    [false, undefined, "pb-3", undefined],
    [false, IOS_ACCESSORY_BAR_PX, "pb-3", undefined],
    [true, undefined, "pb-0", undefined],
    [true, 0, "pb-0", undefined],
    [true, IOS_ACCESSORY_BAR_PX, "pb-0", { paddingBottom: IOS_ACCESSORY_BAR_PX }],
  ])("keyboardOpen=%s accessoryBarPx=%s", (keyboardOpen, accessoryBarPx, padding, style) => {
    const layout = composerWrapperLayout({ keyboardOpen, accessoryBarPx });
    const classes = layout.className.split(" ");
    for (const c of [...BASE, padding]) expect(classes).toContain(c);
    expect(classes).not.toContain(padding === "pb-3" ? "pb-0" : "pb-3");
    expect(layout.style).toEqual(style);
  });
});

const mounted: HTMLTextAreaElement[] = [];

function textareaRef(value: string, start: number, end = start) {
  const ta = document.createElement("textarea");
  ta.value = value;
  ta.selectionStart = start;
  ta.selectionEnd = end;
  document.body.appendChild(ta);
  mounted.push(ta);
  return { current: ta } as React.RefObject<HTMLTextAreaElement | null>;
}

/** Records each input event and the caret at dispatch time. */
function recordInputs(ta: HTMLTextAreaElement) {
  const events: { event: InputEvent; caret: number | null }[] = [];
  ta.addEventListener("input", (e) => events.push({ event: e as InputEvent, caret: ta.selectionStart }));
  return events;
}

afterEach(() => {
  for (const ta of mounted.splice(0)) ta.remove();
});

describe("caret insertion", () => {
  it.each([
    ["no-op without a textarea", () => insertAtCaret({ current: null }, "@")],
    ["newline no-op without a textarea", () => insertNewlineAtCaret({ current: null })],
    ["slash no-op without a textarea", () => insertSlashCommand({ current: null }, { id: "foo" } as never)],
  ])("%s", (_label, fn) => {
    expect(fn).not.toThrow();
  });

  it.each([
    ["", 0, 0, "@", "@", 1],
    ["hi ", 3, 3, "@", "hi @", 4],
    // Mid-word triggers are padded so detection still fires.
    ["hi", 2, 2, "/", "hi /", 4],
    ["hello", 5, 5, "@", "hello @", 7],
    ["hello world", 6, 11, "@", "hello @", 7],
  ])("insertAtCaret(%j [%i,%i], %j)", (value, start, end, text, expected, caret) => {
    const ref = textareaRef(value, start, end);
    const events = recordInputs(ref.current!);
    insertAtCaret(ref, text);
    expect(ref.current!.value).toBe(expected);
    expect(ref.current!.selectionStart).toBe(caret);
    // The trigger popover needs a real InputEvent carrying inputType and data.
    expect(events).toHaveLength(1);
    expect(events[0]!.event).toBeInstanceOf(InputEvent);
    expect(events[0]!.event.bubbles).toBe(true);
    expect(events[0]!.event.inputType).toBe("insertText");
    expect(events[0]!.event.data).toBe(text);
  });

  it.each([
    ["abcd", 2, 2, "ab\ncd", 3],
    ["abcdef", 1, 4, "a\nef", 2],
  ])("insertNewlineAtCaret(%j [%i,%i])", (value, start, end, expected, caret) => {
    const ref = textareaRef(value, start, end);
    insertNewlineAtCaret(ref);
    expect(ref.current!.value).toBe(expected);
    expect(ref.current!.selectionStart).toBe(caret);
    expect(ref.current!.selectionEnd).toBe(caret);
  });

  it("replaces the caret's slash token with the caret already placed when the event fires", () => {
    const ref = textareaRef("fix /he the bug", 7);
    const events = recordInputs(ref.current!);
    insertSlashCommand(ref, { id: "help" } as never);
    expect(ref.current!.value).toBe("fix /help the bug");
    expect(ref.current!.selectionStart).toBe(10);
    expect(events).toHaveLength(1);
    expect(events[0]!.event.inputType).toBe("insertText");
    expect(events[0]!.event.data).toBe("/help");
    // The primitive reads selectionStart in the same onChange that applies the text.
    expect(events[0]!.caret).toBe(10);
  });
});
