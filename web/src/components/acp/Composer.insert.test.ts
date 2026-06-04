// @vitest-environment jsdom
//
// Regression test for #1149: clicking the @ or / toolbar buttons used
// to leave a duplicate trigger character in the composer because we
// dispatched a generic `Event("input")`. assistant-ui's
// `Unstable_TriggerPopover` keys its trigger detection off
// `InputEvent.inputType` + `data`; without those the popover's
// `removeOnExecute` cannot strip the toolbar-injected character on
// item selection, leaving `@@` / `//` in the input. We now dispatch a
// real `InputEvent` carrying both fields.

import { describe, expect, it } from "vitest";
import { createRef } from "react";

import { insertAtCaret } from "./Composer";

describe("insertAtCaret (#1149)", () => {
  it("dispatches an InputEvent with inputType=insertText and data=text", () => {
    const ta = document.createElement("textarea");
    document.body.appendChild(ta);
    const ref = createRef<HTMLTextAreaElement>();
    // React refs use a writable `current` property in tests like this.
    (ref as { current: HTMLTextAreaElement }).current = ta;

    const events: Event[] = [];
    ta.addEventListener("input", (e) => events.push(e));

    insertAtCaret(ref, "@");

    expect(ta.value).toBe("@");
    expect(events).toHaveLength(1);
    const evt = events[0];
    expect(evt).toBeInstanceOf(InputEvent);
    expect((evt as InputEvent).inputType).toBe("insertText");
    expect((evt as InputEvent).data).toBe("@");
    expect(evt.bubbles).toBe(true);

    document.body.removeChild(ta);
  });

  it("forwards the inserted text in the InputEvent.data field", () => {
    const ta = document.createElement("textarea");
    document.body.appendChild(ta);
    const ref = createRef<HTMLTextAreaElement>();
    (ref as { current: HTMLTextAreaElement }).current = ta;

    const events: InputEvent[] = [];
    ta.addEventListener("input", (e) => events.push(e as InputEvent));

    insertAtCaret(ref, "/");

    expect(ta.value).toBe("/");
    expect(events).toHaveLength(1);
    expect(events[0].inputType).toBe("insertText");
    expect(events[0].data).toBe("/");

    document.body.removeChild(ta);
  });

  it("pads with a leading space when caret is mid-word so trigger detection still fires", () => {
    const ta = document.createElement("textarea");
    document.body.appendChild(ta);
    ta.value = "hello";
    ta.setSelectionRange(5, 5);
    const ref = createRef<HTMLTextAreaElement>();
    (ref as { current: HTMLTextAreaElement }).current = ta;

    insertAtCaret(ref, "@");

    expect(ta.value).toBe("hello @");

    document.body.removeChild(ta);
  });
});
