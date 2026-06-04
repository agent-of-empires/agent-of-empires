// User story: iOS Safari native dictation into the structured view composer
// commits each partial recognition exactly once instead of re-appending
// the prior partial on every event (#1431).
//
// The bug shape on real iOS: WebKit fires `beforeinput` + `input` with
// `inputType: "insertReplacementText"` per partial. WebKit tracks a
// private range pointer into the textarea's text storage, and any JS
// write to `textarea.value` via the property setter invalidates that
// pointer, so the next partial appends instead of replacing. The fix
// detects the burst, suppresses assistant-ui's controlled-input flush
// for the duration, buffers the textarea value, and drains the buffer
// into `composerRuntime.setText` once at burst end (timeout, blur, or
// a non-replacement input event).
//
// Chromium does not reproduce the WebKit pointer-reset itself, so this
// spec exercises the new code path end-to-end and asserts the
// observable invariants:
//   - During the burst, assistant-ui state stays frozen (the localStorage
//     draft, which mirrors composerRuntime state with a 250 ms debounce,
//     does NOT update per replacement event).
//   - After blur, the buffered text flushes into composerRuntime state,
//     and the draft picks up the final dictated phrase exactly once.
//   - The textarea still displays the final phrase exactly once with no
//     duplication artifacts.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

base(
  "iOS dictation commits each partial exactly once, syncs on blur",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-ios-dictation" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === "story-ios-dictation");
      if (!seeded) throw new Error("seeded session 'story-ios-dictation' missing");
      const sessionId = seeded.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
      await waitForStructuredView(page);

      const composer = page.getByRole("textbox", { name: /Send a message/i });
      await composer.click();
      await expect(composer).toBeFocused();

      // Simulate the WebKit dictation emission shape. Each partial:
      //   1. dispatch a cancelable `beforeinput` with
      //      `inputType: "insertReplacementText"` so the composer's
      //      `onBeforeInput` flips the burst flag.
      //   2. set `textarea.value` via the native property setter, the
      //      way WebKit would after the replacement is committed.
      //   3. dispatch a non-cancelable `input` with the same inputType
      //      so React's `onChange` runs and the composer's interceptor
      //      can preventDefault on the composed assistant-ui handler.
      const partials = [
        "open",
        "open the",
        "open the diff",
        "open the diff viewer",
      ];
      for (const partial of partials) {
        await composer.evaluate((el, text) => {
          const ta = el as HTMLTextAreaElement;
          const beforeEvent = new InputEvent("beforeinput", {
            bubbles: true,
            cancelable: true,
            inputType: "insertReplacementText",
            data: text,
          });
          ta.dispatchEvent(beforeEvent);
          const setter = Object.getOwnPropertyDescriptor(
            HTMLTextAreaElement.prototype,
            "value",
          )?.set;
          setter?.call(ta, text);
          const inputEvent = new InputEvent("input", {
            bubbles: true,
            cancelable: false,
            inputType: "insertReplacementText",
            data: text,
          });
          ta.dispatchEvent(inputEvent);
        }, partial);
      }

      // While the burst is active the composer should be holding the
      // latest textarea value in its ref, NOT pushing it through
      // composerRuntime. The localStorage draft mirrors composerRuntime
      // state, so it should still be empty (or null) immediately after
      // the burst events.
      const draftKey = `acp:draft:${sessionId}`;
      const draftMid = await page.evaluate(
        (k) => localStorage.getItem(k),
        draftKey,
      );
      expect(draftMid ?? "").toBe("");

      // Textarea displays the final dictated phrase exactly once, no
      // duplication artifacts from the simulated replacement chain.
      await expect(composer).toHaveValue("open the diff viewer");

      // Blur flushes the buffer into composerRuntime. The draft
      // subscriber then writes the final phrase to localStorage on its
      // 250 ms debounce.
      await composer.blur();

      await expect
        .poll(
          async () =>
            await page.evaluate((k) => localStorage.getItem(k), draftKey),
          { timeout: 5_000 },
        )
        .toBe("open the diff viewer");

      await expect(composer).toHaveValue("open the diff viewer");
    } finally {
      await serve.stop();
    }
  },
);

base(
  "regular typing after dictation is unaffected",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-ios-dictation-then-type" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find(
        (s) => s.title === "story-ios-dictation-then-type",
      );
      if (!seeded)
        throw new Error("seeded session 'story-ios-dictation-then-type' missing");
      const sessionId = seeded.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
      await waitForStructuredView(page);

      const composer = page.getByRole("textbox", { name: /Send a message/i });
      await composer.click();

      // One dictation burst, then a real keystroke. The keystroke fires
      // `beforeinput` + `input` with `inputType: "insertText"`, which
      // should end the burst and let the keystroke flow normally to
      // assistant-ui state. The final composer state must include both
      // the dictated text and the appended keystroke text.
      await composer.evaluate((el) => {
        const ta = el as HTMLTextAreaElement;
        ta.dispatchEvent(
          new InputEvent("beforeinput", {
            bubbles: true,
            cancelable: true,
            inputType: "insertReplacementText",
            data: "hello",
          }),
        );
        const setter = Object.getOwnPropertyDescriptor(
          HTMLTextAreaElement.prototype,
          "value",
        )?.set;
        setter?.call(ta, "hello");
        ta.dispatchEvent(
          new InputEvent("input", {
            bubbles: true,
            cancelable: false,
            inputType: "insertReplacementText",
            data: "hello",
          }),
        );
      });

      // Place the caret at end so the appended keystroke lands after
      // the dictated text, mirroring what a real user would do.
      await composer.evaluate((el) => {
        const ta = el as HTMLTextAreaElement;
        ta.setSelectionRange(ta.value.length, ta.value.length);
      });

      // Type a single character via Playwright; this simulates a real
      // keyboard keystroke and exercises the burst-end-on-non-replacement
      // branch of the state machine.
      await composer.pressSequentially("!");

      await expect(composer).toHaveValue("hello!");

      await composer.blur();

      const draftKey = `acp:draft:${sessionId}`;
      await expect
        .poll(
          async () =>
            await page.evaluate((k) => localStorage.getItem(k), draftKey),
          { timeout: 5_000 },
        )
        .toBe("hello!");
    } finally {
      await serve.stop();
    }
  },
);
