// @vitest-environment jsdom
//
// Unit coverage for the exported pure helpers in Composer.tsx: the
// Enter / beforeinput decision matrices, the wrapper-layout helper, the
// caret insertion helpers, and the slash-command insertion helper. These
// are kept side-effect-free precisely so they can be exercised without
// mounting the assistant-ui runtime; see the doc comments on each helper.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  composerWrapperLayout,
  decideBeforeInputAction,
  decideEnterAction,
  insertAtCaret,
  insertNewlineAtCaret,
  insertSlashCommand,
} from "../Composer";

function enterEvent(over: Partial<Parameters<typeof decideEnterAction>[0]> = {}) {
  return {
    key: "Enter",
    shiftKey: false,
    ctrlKey: false,
    metaKey: false,
    isComposing: false,
    ...over,
  };
}

describe("decideEnterAction", () => {
  const desktop = { isMobile: false, turnActive: false };

  it("returns default for non-Enter keys", () => {
    expect(decideEnterAction(enterEvent({ key: "a" }), desktop)).toBe("default");
  });

  it("returns default while IME composing", () => {
    expect(decideEnterAction(enterEvent({ isComposing: true }), desktop)).toBe("default");
  });

  it("returns default for Shift+Enter", () => {
    expect(decideEnterAction(enterEvent({ shiftKey: true }), desktop)).toBe("default");
  });

  it("returns default for Ctrl+Enter", () => {
    expect(decideEnterAction(enterEvent({ ctrlKey: true }), desktop)).toBe("default");
  });

  it("returns default for Meta+Enter", () => {
    expect(decideEnterAction(enterEvent({ metaKey: true }), desktop)).toBe("default");
  });

  it("returns default on mobile even when a turn is active", () => {
    expect(decideEnterAction(enterEvent(), { isMobile: true, turnActive: true })).toBe("default");
  });

  it("returns send on desktop when a turn is active (mid-turn queue path)", () => {
    expect(decideEnterAction(enterEvent(), { isMobile: false, turnActive: true })).toBe("send");
  });

  it("returns default on desktop plain Enter with no active turn", () => {
    expect(decideEnterAction(enterEvent(), { isMobile: false, turnActive: false })).toBe("default");
  });
});

describe("decideBeforeInputAction", () => {
  it("returns default on desktop regardless of inputType", () => {
    expect(decideBeforeInputAction("insertLineBreak", false, { isMobile: false })).toBe("default");
    expect(decideBeforeInputAction("insertParagraph", false, { isMobile: false })).toBe("default");
  });

  it("returns default on mobile while composing", () => {
    expect(decideBeforeInputAction("insertLineBreak", true, { isMobile: true })).toBe("default");
  });

  it("returns default on mobile for unrelated inputTypes", () => {
    expect(decideBeforeInputAction("insertText", false, { isMobile: true })).toBe("default");
  });

  it("returns newline on mobile for insertLineBreak", () => {
    expect(decideBeforeInputAction("insertLineBreak", false, { isMobile: true })).toBe("newline");
  });

  it("returns newline on mobile for insertParagraph", () => {
    expect(decideBeforeInputAction("insertParagraph", false, { isMobile: true })).toBe("newline");
  });
});

describe("composerWrapperLayout", () => {
  it("uses pb-3 and no bottom-margin style when the keyboard is closed", () => {
    const layout = composerWrapperLayout({ keyboardOpen: false });
    expect(layout.className).toContain("pb-3");
    expect(layout.className).not.toContain("pb-0");
    expect(layout.style).toBeUndefined();
  });

  it("uses pb-0 and a negative safe-area bottom margin when the keyboard is open", () => {
    const layout = composerWrapperLayout({ keyboardOpen: true });
    expect(layout.className).toContain("pb-0");
    expect(layout.className).not.toContain("pb-3");
    expect(layout.style).toEqual({ marginBottom: "calc(-1 * env(safe-area-inset-bottom))" });
  });
});

const mountedTextareas: HTMLTextAreaElement[] = [];

function textareaRef(value: string, start: number, end = start) {
  const ta = document.createElement("textarea");
  ta.value = value;
  ta.selectionStart = start;
  ta.selectionEnd = end;
  document.body.appendChild(ta);
  mountedTextareas.push(ta);
  return { current: ta } as React.RefObject<HTMLTextAreaElement | null>;
}

afterEach(() => {
  for (const ta of mountedTextareas.splice(0)) ta.remove();
});

describe("insertNewlineAtCaret", () => {
  it("is a no-op when the ref is empty", () => {
    expect(() => insertNewlineAtCaret({ current: null })).not.toThrow();
  });

  it("inserts a newline at the caret and advances the caret past it", () => {
    const ref = textareaRef("abcd", 2);
    insertNewlineAtCaret(ref);
    const ta = ref.current!;
    expect(ta.value).toBe("ab\ncd");
    expect(ta.selectionStart).toBe(3);
    expect(ta.selectionEnd).toBe(3);
  });

  it("replaces a selection range with a newline", () => {
    const ref = textareaRef("abcdef", 1, 4);
    insertNewlineAtCaret(ref);
    const ta = ref.current!;
    expect(ta.value).toBe("a\nef");
    expect(ta.selectionStart).toBe(2);
  });
});

describe("insertAtCaret", () => {
  it("is a no-op when the ref is empty", () => {
    expect(() => insertAtCaret({ current: null }, "@")).not.toThrow();
  });

  it("inserts at the start without padding", () => {
    const ref = textareaRef("", 0);
    insertAtCaret(ref, "@");
    const ta = ref.current!;
    expect(ta.value).toBe("@");
    expect(ta.selectionStart).toBe(1);
  });

  it("inserts after whitespace without adding extra padding", () => {
    const ref = textareaRef("hi ", 3);
    insertAtCaret(ref, "@");
    expect(ref.current!.value).toBe("hi @");
    expect(ref.current!.selectionStart).toBe(4);
  });

  it("pads with a leading space when mid-word", () => {
    const ref = textareaRef("hi", 2);
    insertAtCaret(ref, "/");
    expect(ref.current!.value).toBe("hi /");
    expect(ref.current!.selectionStart).toBe(4);
  });

  it("replaces a selection and inserts text", () => {
    const ref = textareaRef("hello world", 6, 11);
    insertAtCaret(ref, "@");
    // before = "hello " ends in whitespace, so no padding
    expect(ref.current!.value).toBe("hello @");
    expect(ref.current!.selectionStart).toBe(7);
  });
});

describe("insertSlashCommand", () => {
  it("is a no-op when the runtime is missing", () => {
    expect(() => insertSlashCommand(null as never, { id: "foo" } as never)).not.toThrow();
  });

  it("sets the canonical /<id> form with a trailing space when buffer is empty", () => {
    const setText = vi.fn();
    const runtime = { getState: () => ({ text: "" }), setText } as never;
    insertSlashCommand(runtime, { id: "compact" } as never);
    expect(setText).toHaveBeenCalledWith("/compact ");
  });

  it("appends a separating space when the buffer does not end in whitespace", () => {
    const setText = vi.fn();
    const runtime = { getState: () => ({ text: "hello" }), setText } as never;
    insertSlashCommand(runtime, { id: "foo" } as never);
    expect(setText).toHaveBeenCalledWith("hello /foo ");
  });

  it("does not double-space when the buffer already ends in whitespace", () => {
    const setText = vi.fn();
    const runtime = { getState: () => ({ text: "hello " }), setText } as never;
    insertSlashCommand(runtime, { id: "foo" } as never);
    expect(setText).toHaveBeenCalledWith("hello /foo ");
  });
});
