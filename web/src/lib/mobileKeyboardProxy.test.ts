// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  clearMobileKeyboardProxyInput,
  deliverMobileKeyboardProxyInput,
  forwardTerminalBeforeInput,
  invalidateRetainedImeContext,
  registerMobileKeyboardProxyReceiver,
} from "./mobileKeyboardProxy";

afterEach(clearMobileKeyboardProxyInput);

// `delivered` is what the receiver reports back: false means the pane refused
// the edit, so the textarea must not record it either.
function beforeInput(target: HTMLTextAreaElement, init: InputEventInit, delivered = true) {
  const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, ...init });
  const deliver = vi.fn(() => delivered);
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

  // The line-break branch runs for other targets too (the helper is
  // target-agnostic); it must not throw on a non-textarea.
  it("tolerates a non-textarea target on line breaks", () => {
    const div = document.createElement("div");
    const deliver = vi.fn(() => true);
    const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertLineBreak" });
    Object.defineProperty(ev, "target", { value: div });
    expect(() => forwardTerminalBeforeInput(ev, deliver)).not.toThrow();
    expect(ev.defaultPrevented).toBe(true);
  });

  it("swallows pastes so they never enter the textarea", () => {
    const ta = document.createElement("textarea");
    const { ev, deliver } = beforeInput(ta, { inputType: "insertFromPaste", data: "a\nb" });
    expect(deliver).toHaveBeenCalledWith({ inputType: "insertFromPaste", data: "a\nb", isComposing: false });
    expect(ev.defaultPrevented).toBe(true);
  });

  // A Ctrl chord sends a control code instead of the letter, and a read-only
  // viewer's keystroke is dropped outright. Either way the pane never got the
  // text, so retaining it would make the next rewrite's delete eat a
  // character the user did not type. See #3877.
  it("cancels an insert the pane refused, so the textarea stays empty", () => {
    const ta = document.createElement("textarea");
    const { ev, deliver } = beforeInput(ta, { inputType: "insertText", data: "c" }, false);
    expect(deliver).toHaveBeenCalledWith({ inputType: "insertText", data: "c", isComposing: false });
    expect(ev.defaultPrevented).toBe(true);
  });

  it("cancels a refused delete and drops the retained text", () => {
    const ta = document.createElement("textarea");
    ta.value = "\uadf8";
    const { ev } = beforeInput(ta, { inputType: "deleteContentBackward" }, false);
    expect(ev.defaultPrevented).toBe(true);
    expect(ta.value).toBe("");
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

  // The overflow guard: a bound queue rejects further edits instead of
  // growing without limit while no terminal is mounted.
  it("rejects input past the queue bound", () => {
    for (let i = 0; i < 128; i++) {
      const ok = deliverMobileKeyboardProxyInput({ inputType: "insertText", data: `x${i}`, isComposing: false });
      expect(ok).toBe(true);
    }
    expect(deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "over", isComposing: false })).toBe(false);
  });

  it("drops queued input at a session boundary", () => {
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "old", isComposing: false });
    clearMobileKeyboardProxyInput();
    const receive = vi.fn();
    registerMobileKeyboardProxyReceiver(receive);
    expect(receive).not.toHaveBeenCalled();
  });

  // #3885 case 3: a queued edit the receiver refuses must not survive in the
  // proxy textarea. The drain replays the queue verbatim; when the pane
  // refuses an edit, the shadow that holds it no longer mirrors the line.
  it("clears the proxy when a drained queued edit is refused", () => {
    document.body.innerHTML = "<textarea data-keyboard-proxy></textarea>";
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]")!;
    proxy.value = "ㅎ";
    expect(proxy.value).toBe("ㅎ");
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "가", isComposing: false });

    const receive = vi.fn(() => false);
    const unregister = registerMobileKeyboardProxyReceiver(receive);
    expect(receive).toHaveBeenCalledWith({ inputType: "insertText", data: "가", isComposing: false });
    // The queued edit the pane refused must not leave the proxy holding "ㅎ".
    expect(proxy.value).toBe("");
    unregister();
    document.body.innerHTML = "";
  });

  // A drained queue with an accepting receiver leaves the proxy alone.
  it("keeps the proxy content when drained edits are accepted", () => {
    document.body.innerHTML = "<textarea data-keyboard-proxy></textarea>";
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]")!;
    proxy.value = "ㅎ";
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "가", isComposing: false });
    const receive = vi.fn(() => true);
    const unregister = registerMobileKeyboardProxyReceiver(receive);
    expect(receive).toHaveBeenCalled();
    expect(proxy.value).toBe("ㅎ");
    unregister();
    document.body.innerHTML = "";
  });

  // Unregistering a receiver that is not the current one must not detach
  // the live receiver.
  it("keeps the current receiver when an older cleanup runs", () => {
    const first = vi.fn(() => true);
    const stop1 = registerMobileKeyboardProxyReceiver(first);
    const second = vi.fn(() => true);
    const stop2 = registerMobileKeyboardProxyReceiver(second);
    stop1();
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "x", isComposing: false });
    expect(second).toHaveBeenCalledWith({ inputType: "insertText", data: "x", isComposing: false });
    expect(first).not.toHaveBeenCalledWith({ inputType: "insertText", data: "x", isComposing: false });
    stop2();
  });
});

// #3885: a refused edit clears the target when it is a textarea. The guard
// must also tolerate a non-textarea target (the handler is wired per-input,
// but nothing in the helper's contract guarantees it).
describe("forwardTerminalBeforeInput refused-edit target guard", () => {
  it("prevents refused edits on non-textarea targets", () => {
    const div = document.createElement("div");
    const deliver = vi.fn(() => false);
    const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: "c" });
    Object.defineProperty(ev, "target", { value: div });
    forwardTerminalBeforeInput(ev, deliver);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("clears the target textarea when the pane refuses the edit", () => {
    const ta = document.createElement("textarea");
    ta.value = "한";
    const deliver = vi.fn(() => false);
    const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: "c" });
    Object.defineProperty(ev, "target", { value: ta });
    forwardTerminalBeforeInput(ev, deliver);
    expect(ta.value).toBe("");
    expect(ev.defaultPrevented).toBe(true);
  });
});

describe("invalidateRetainedImeContext", () => {
  afterEach(() => {
    document.body.innerHTML = "";
  });

  // Either hidden input can hold focus, and a click handler has no event
  // target to say which, so both are cleared.
  it("clears the live terminal's input and App's persistent proxy", () => {
    const proxy = document.createElement("textarea");
    proxy.setAttribute("data-keyboard-proxy", "");
    proxy.value = "ㅎ";
    document.body.append(proxy);
    const local = document.createElement("textarea");
    local.value = "ㅎ";

    invalidateRetainedImeContext(local);

    expect(local.value).toBe("");
    expect(proxy.value).toBe("");
  });

  // #3885 case 2 shape: the no-argument call must not be relied on for the
  // live terminal's own textarea. (The spec-level regression covers the
  // upload completion; this pins the helper contract the fix rests on.)
  it("clears only the proxy with no element passed", () => {
    const proxy = document.createElement("textarea");
    proxy.setAttribute("data-keyboard-proxy", "");
    proxy.value = "ㅎ";
    document.body.append(proxy);
    const local = document.createElement("textarea");
    local.value = "ㅎ";

    invalidateRetainedImeContext();

    expect(local.value).toBe("ㅎ");
    expect(proxy.value).toBe("");
  });

  it("clears the proxy with no element passed, and tolerates a missing one", () => {
    const proxy = document.createElement("textarea");
    proxy.setAttribute("data-keyboard-proxy", "");
    proxy.value = "ㅎ";
    document.body.append(proxy);

    invalidateRetainedImeContext();
    expect(proxy.value).toBe("");

    proxy.remove();
    expect(() => invalidateRetainedImeContext(null)).not.toThrow();
  });
});
