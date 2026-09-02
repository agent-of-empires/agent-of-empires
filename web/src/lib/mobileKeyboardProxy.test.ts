// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  clearMobileKeyboardProxyInput,
  deliverMobileKeyboardProxyInput,
  forwardTerminalBeforeInput,
  registerMobileKeyboardProxyReceiver,
} from "./mobileKeyboardProxy";

afterEach(clearMobileKeyboardProxyInput);

function beforeInput(target: HTMLTextAreaElement, init: InputEventInit) {
  const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, ...init });
  const deliver = vi.fn();
  target.addEventListener("beforeinput", (e) => forwardTerminalBeforeInput(e as InputEvent, deliver), { once: true });
  target.dispatchEvent(ev);
  return { ev, deliver };
}

describe("forwardTerminalBeforeInput", () => {
  // iOS WebKit's Korean keyboard rewrites the trailing syllable as
  // deleteContentBackward + insertText with no composition events, and skips
  // the delete when the textarea is empty. Text edits must therefore land in
  // the textarea (default NOT prevented) so the next delete is observable.
  it("forwards insertText and lets it land in the textarea", () => {
    const ta = document.createElement("textarea");
    const { ev, deliver } = beforeInput(ta, { inputType: "insertText", data: "ㅎ" });
    expect(deliver).toHaveBeenCalledWith({ inputType: "insertText", data: "ㅎ", isComposing: false });
    expect(ev.defaultPrevented).toBe(false);
  });

  it("forwards deleteContentBackward and lets the textarea shrink", () => {
    const ta = document.createElement("textarea");
    ta.value = "ㅎ";
    const { ev, deliver } = beforeInput(ta, { inputType: "deleteContentBackward" });
    expect(deliver).toHaveBeenCalledWith({ inputType: "deleteContentBackward", data: null, isComposing: false });
    expect(ev.defaultPrevented).toBe(false);
  });

  it("swallows line breaks and drops the accumulated IME context", () => {
    const ta = document.createElement("textarea");
    ta.value = "한국어";
    const { ev, deliver } = beforeInput(ta, { inputType: "insertLineBreak" });
    expect(deliver).toHaveBeenCalledWith({ inputType: "insertLineBreak", data: null, isComposing: false });
    expect(ev.defaultPrevented).toBe(true);
    expect(ta.value).toBe("");
  });

  it("swallows pastes so they never enter the textarea", () => {
    const ta = document.createElement("textarea");
    const { ev, deliver } = beforeInput(ta, { inputType: "insertFromPaste", data: "a\nb" });
    expect(deliver).toHaveBeenCalledWith({ inputType: "insertFromPaste", data: "a\nb", isComposing: false });
    expect(ev.defaultPrevented).toBe(true);
  });

  it("ignores other input types", () => {
    const ta = document.createElement("textarea");
    const { ev, deliver } = beforeInput(ta, { inputType: "insertReplacementText", data: "x" });
    expect(deliver).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(false);
  });
});

describe("mobile keyboard proxy", () => {
  it("delivers input buffered while a session is mounting", () => {
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "first", isComposing: false });
    const receive = vi.fn();
    registerMobileKeyboardProxyReceiver(receive);
    expect(receive).toHaveBeenCalledWith({ inputType: "insertText", data: "first", isComposing: false });
  });

  it("drops queued input at a session boundary", () => {
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "old", isComposing: false });
    clearMobileKeyboardProxyInput();
    const receive = vi.fn();
    registerMobileKeyboardProxyReceiver(receive);
    expect(receive).not.toHaveBeenCalled();
  });
});
