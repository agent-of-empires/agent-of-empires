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

describe("key decisions", () => {
  it("decideEnterAction", () => {
    for (const [over, isMobile, turnActive, expected] of [
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
    ] as const) {
      expect(decideEnterAction({ ...keys, ...over }, { isMobile, turnActive }), JSON.stringify(over)).toBe(expected);
    }
  });

  it("decideArrowRecall", () => {
    const up = { ...keys, key: "ArrowUp" };
    const down = { ...keys, key: "ArrowDown" };
    for (const [event, caretAtStart, browsing, queueLen, expected] of [
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
    ] as const) {
      expect(
        decideArrowRecall(event, { caretAtStart, browsing, queueLen }),
        JSON.stringify([event, caretAtStart, browsing]),
      ).toBe(expected);
    }
  });

  it("decideBeforeInputAction", () => {
    for (const [inputType, isComposing, isMobile, expected] of [
      ["insertLineBreak", false, true, "newline"],
      ["insertParagraph", false, true, "newline"],
      ["insertText", false, true, "default"],
      ["deleteContentBackward", false, true, "default"],
      ["insertLineBreak", false, false, "default"],
      ["insertParagraph", false, false, "default"],
      ["insertLineBreak", true, true, "default"],
    ] as const) {
      expect(
        decideBeforeInputAction(inputType, isComposing, { isMobile }),
        `${inputType} ${isComposing} ${isMobile}`,
      ).toBe(expected);
    }
  });
});

describe("composerWrapperLayout", () => {
  it("pads for the keyboard and the iOS accessory bar", () => {
    for (const [keyboardOpen, accessoryBarPx, padding, style] of [
      [false, undefined, "pb-3", undefined],
      [false, IOS_ACCESSORY_BAR_PX, "pb-3", undefined],
      [true, undefined, "pb-0", undefined],
      [true, 0, "pb-0", undefined],
    ] as const) {
      const label = `${keyboardOpen} ${accessoryBarPx}`;
      const classes = composerWrapperLayout({ keyboardOpen, accessoryBarPx }).className.split(" ");
      expect(classes, label).toContain(padding);
      expect(classes, label).not.toContain(padding === "pb-3" ? "pb-0" : "pb-3");
      expect(composerWrapperLayout({ keyboardOpen, accessoryBarPx }).style, label).toEqual(style);
    }
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
  it("is a no-op without a textarea", () => {
    expect(() => insertAtCaret({ current: null }, "@")).not.toThrow();
    expect(() => insertNewlineAtCaret({ current: null })).not.toThrow();
    expect(() => insertSlashCommand({ current: null }, { id: "foo" } as never)).not.toThrow();
  });

  it("insertAtCaret replaces the selection and emits one insertText InputEvent", () => {
    for (const [value, start, end, text, expected, caret] of [
      ["", 0, 0, "@", "@", 1],
      ["hi ", 3, 3, "@", "hi @", 4],
      // Mid-word triggers are padded so detection still fires.
      ["hi", 2, 2, "/", "hi /", 4],
      ["hello", 5, 5, "@", "hello @", 7],
      ["hello world", 6, 11, "@", "hello @", 7],
    ] as const) {
      const ref = textareaRef(value, start, end);
      const events = recordInputs(ref.current!);
      insertAtCaret(ref, text);
      expect(ref.current!.value).toBe(expected);
      expect(ref.current!.selectionStart, value).toBe(caret);
      // The trigger popover needs a real InputEvent carrying inputType and data.
      expect(events, value).toHaveLength(1);
      expect(events[0]!.event).toBeInstanceOf(InputEvent);
      expect(events[0]!.event.bubbles).toBe(true);
      expect(events[0]!.event.inputType).toBe("insertText");
      expect(events[0]!.event.data).toBe(text);
    }
  });

  it("insertNewlineAtCaret replaces the selection and collapses the caret", () => {
    const ref = textareaRef("abcdef", 1, 4);
    insertNewlineAtCaret(ref);
    expect(ref.current!.value).toBe("a\nef");
    expect(ref.current!.selectionStart).toBe(2);
    expect(ref.current!.selectionEnd).toBe(2);
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
