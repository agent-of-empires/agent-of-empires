// @vitest-environment jsdom
// The hidden input sends its value's diff, so an IME or dictation rewriting text the pane already has sends only
// what changed (#3746 SwiftKey word commits, iOS dictation hypotheses).

import { describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { invalidateRetainedImeContext } from "../../lib/mobileKeyboardProxy";
import { installResizeObserver, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

interface Term {
  /** Plain edits, one per character, as a soft keyboard sends them. */
  type: (text: string) => void;
  /** A retroactive composition: SwiftKey (#3746) re-wraps the word before the caret and commits `data`. */
  compose: (data: string) => void;
  /** A composition that starts at the caret. */
  composeFresh: (data: string) => void;
  /** iOS dictation: each hypothesis replaces the previous one in place. */
  dictate: (hypotheses: string[]) => void;
  input: (inputType: "deleteContentBackward" | "insertParagraph") => void;
  /** A toolbar button, which writes past this component to live.sendData. */
  toolbar: (data: string) => void;
  sent: () => string[];
}

// `accepted` models useLiveTerminal.sendData's contract: false is a keystroke
// the pane never receives (a confirmed non-owner, or a full pending queue).
function renderTerm(accepted = true): Term {
  const sendData = vi.fn((_data: string) => accepted);
  const input = renderLiveTerminal({ sendData }).input();
  // Edits land as the browser applies them: the value changes, then `input` fires.
  const edit = (inputType: string, value: string, isComposing = false) => {
    input.value = value;
    input.dispatchEvent(new InputEvent("input", { inputType, isComposing, bubbles: true }));
  };
  const composeOver = (from: number, data: string) => {
    fireEvent.compositionStart(input);
    edit("insertCompositionText", input.value.slice(0, from) + data, true);
    fireEvent.compositionEnd(input, { data });
  };
  return {
    type: (text) => {
      for (const ch of text) edit("insertText", input.value + ch);
    },
    compose: (data) => composeOver(input.value.length - (/\S*$/.exec(input.value)?.[0].length ?? 0), data),
    composeFresh: (data) => composeOver(input.value.length, data),
    dictate: (hypotheses) => {
      const from = input.value.length;
      for (const h of hypotheses) edit("insertReplacementText", input.value.slice(0, from) + h);
    },
    input: (inputType) => {
      if (inputType === "deleteContentBackward") edit(inputType, Array.from(input.value).slice(0, -1).join(""));
      else input.dispatchEvent(new InputEvent("beforeinput", { inputType, bubbles: true, cancelable: true }));
    },
    toolbar: (data) => {
      invalidateRetainedImeContext(input);
      sendData(data);
    },
    sent: () => sendData.mock.calls.map(([d]: [string]) => d),
  };
}

describe("MobileLiveTerminal IME and dictation rewrites", () => {
  const cases: { name: string; accepted?: boolean; run: (t: Term) => void; sent: string[] }[] = [
    {
      name: "sends a SwiftKey word once when the composition repeats it",
      // The reporter's trace: "test" typed plainly, composed on space, then " ".
      run: (t) => {
        t.type("test");
        t.compose("test");
        t.type(" ");
      },
      sent: ["t", "e", "s", "t", " "],
    },
    {
      name: "sends only the tail when the composition extends the typed word",
      run: (t) => {
        t.type("tes");
        t.compose("test");
      },
      sent: ["t", "e", "s", "t"],
    },
    {
      name: "sends the word once when a second composition commits it",
      run: (t) => {
        t.type("tes");
        t.compose("test");
        t.compose("test");
        t.type(" ");
      },
      sent: ["t", "e", "s", "t", " "],
    },
    {
      name: "keeps stripping a word typed on after a composition",
      run: (t) => {
        t.type("test");
        t.compose("test");
        t.type("s");
        t.compose("tests");
        t.type(" ");
      },
      sent: ["t", "e", "s", "t", "s", " "],
    },
    {
      // A path or token can outrun any fixed cap on the tracked word.
      name: "strips a word longer than any cap on the tracked run",
      run: (t) => {
        const word = "a".repeat(70);
        t.type(word);
        t.compose(word);
        t.type(" ");
      },
      sent: [...Array.from({ length: 70 }, () => "a"), " "],
    },
    {
      // Backspacing an emoji must not leave half a surrogate pair behind.
      name: "tracks a backspace over a non-BMP character",
      run: (t) => {
        t.type("hi\u{1F642}");
        t.input("deleteContentBackward");
        t.compose("hi");
      },
      sent: ["h", "i", "\u{1F642}", "\x7f"],
    },
    {
      // A read-only viewer's keystrokes are dropped, so the pane never got the
      // word and the composition that follows a take-over must be sent whole.
      name: "does not record input the pane never received",
      accepted: false,
      run: (t) => {
        t.type("test");
        t.compose("test");
      },
      sent: ["t", "e", "s", "t", "test"],
    },
    {
      // jerome-benoit on #3751: compositionend.data describes only its own
      // session, so a composition that starts here is not a replacement.
      name: "sends a fresh composition that merely shares the typed prefix",
      run: (t) => {
        t.type("a");
        t.composeFresh("android");
      },
      sent: ["a", "android"],
    },
    {
      // The toolbar writes past this component straight to live.sendData.
      name: "forgets the word after a toolbar interrupt",
      run: (t) => {
        t.type("test");
        t.toolbar("\x03");
        t.type("test");
        t.compose("test");
      },
      sent: ["t", "e", "s", "t", "\x03", "t", "e", "s", "t"],
    },
    {
      name: "sends a composed word that does not continue what was typed",
      run: (t) => {
        t.type("a");
        t.composeFresh("日本");
      },
      sent: ["a", "日本"],
    },
    {
      // A composition that stood on its own is not a typed word under the caret, so the next one must reach the
      // pane whole even when it repeats it.
      name: "sends a character composed twice in a row",
      run: (t) => {
        t.composeFresh("a");
        t.composeFresh("a");
      },
      sent: ["a", "a"],
    },
    {
      name: "forgets the word once Enter has ended the line",
      run: (t) => {
        t.type("ls");
        t.input("insertParagraph");
        t.type("ls");
        t.compose("ls");
      },
      sent: ["l", "s", "\r", "l", "s"],
    },
    {
      name: "tracks a backspace before the composition arrives",
      run: (t) => {
        t.type("test");
        t.input("deleteContentBackward");
        t.compose("tes");
      },
      sent: ["t", "e", "s", "t", "\x7f"],
    },
    {
      name: "sends an iOS Korean syllable rewrite as DEL and the new syllable",
      run: (t) => {
        t.type("ㅎ");
        t.input("deleteContentBackward");
        t.type("하");
      },
      sent: ["ㅎ", "\x7f", "하"],
    },
    {
      // The pane received "thithisthis is..." when each hypothesis was forwarded whole.
      name: "sends each dictation hypothesis once, and a final correction as DELs and the retyped tail",
      run: (t) => {
        t.type("ok ");
        t.dictate(["thi", "this", "this is", "this is a test", "This is a test."]);
      },
      sent: ["o", "k", " ", "thi", "s", " is", " a test", "\x7f".repeat(14) + "This is a test."],
    },
  ];

  for (const c of cases) {
    it(c.name, () => {
      const t = renderTerm(c.accepted);
      c.run(t);
      expect(t.sent()).toEqual(c.sent);
    });
  }
});
