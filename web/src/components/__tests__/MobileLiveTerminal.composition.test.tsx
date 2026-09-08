// @vitest-environment jsdom
//
// Android IME word handling in the live terminal (#3746). SwiftKey spells a
// word out as plain `insertText` edits and then, when space is pressed, wraps
// that same word in a composition after the fact, so `compositionend` carries
// it a second time and "test" reached the pane as "testtest". The traced
// sequence from the reporter's device is the first case below. Only the part
// of the composed word the pane has not already seen may be sent, and typing
// without a composition must be untouched.

import { createRef } from "react";
import { describe, expect, it, vi, beforeAll } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
});

const frame: LiveFrame = {
  content: "$ \n",
  rows: 3,
  history: 1000,
  cursor: null,
  altScreen: false,
  mouse: false,
  mouseSgr: false,
  pane0: null,
};

interface Term {
  /** Plain edits, one per character, as a soft keyboard sends them. */
  type: (text: string) => void;
  /** A retroactive composition: its first update carries the word already
   *  typed, as SwiftKey's trace on #3746 does, then it ends with `data`. */
  compose: (data: string) => void;
  /** A composition whose first update carries `first`, which is what decides
   *  whether it read as taking over the typed word or standing on its own. */
  composeUpdating: (first: string, data: string) => void;
  input: (inputType: string) => void;
  /** A toolbar button, which writes past this component to live.sendData. */
  toolbar: (data: string) => void;
  sent: () => string[];
}

// `accepted` models useLiveTerminal.sendData's contract: false is a keystroke
// the pane never receives (a confirmed non-owner, or a full pending queue).
function renderTerm(accepted = true): Term {
  const inputRef = createRef<HTMLTextAreaElement>();
  // The hook clears the shared word on every write; that is what makes a
  // toolbar button invalidate the run this component is tracking.
  const typedWordRef = { current: "" };
  const sendData = vi.fn((_data: string) => {
    typedWordRef.current = "";
    return accepted;
  });
  render(
    <MobileLiveTerminal
      frame={frame}
      connected
      active
      reading={false}
      sendResize={vi.fn()}
      setWindow={vi.fn()}
      setCadence={vi.fn()}
      enterReading={vi.fn()}
      returnToLive={vi.fn()}
      sendData={sendData}
      typedWordRef={typedWordRef}
      uploadPastedImage={vi.fn().mockResolvedValue(null)}
      forwardWheel={vi.fn()}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
      keyboardOpen={false}
    />,
  );
  const input = inputRef.current!;
  const beforeInput = (inputType: string, data: string | null) =>
    input.dispatchEvent(new InputEvent("beforeinput", { inputType, data, bubbles: true, cancelable: true }));
  return {
    type: (text) => {
      for (const ch of text) beforeInput("insertText", ch);
    },
    compose: (data) => {
      fireEvent.compositionStart(input);
      fireEvent.compositionUpdate(input, { data: typedWordRef.current });
      fireEvent.compositionEnd(input, { data });
    },
    composeUpdating: (first, data) => {
      fireEvent.compositionStart(input);
      fireEvent.compositionUpdate(input, { data: first });
      fireEvent.compositionEnd(input, { data });
    },
    input: (inputType) => beforeInput(inputType, null),
    toolbar: (data) => sendData(data),
    sent: () => sendData.mock.calls.map(([d]: [string]) => d),
  };
}

describe("MobileLiveTerminal Android IME word commits", () => {
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
      name: "keeps every word of a sentence typed that way",
      run: (t) => {
        t.type("hi");
        t.compose("hi");
        t.type(" you");
        t.compose("you");
        t.type(" ");
      },
      sent: ["h", "i", " ", "y", "o", "u", " "],
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
        t.composeUpdating("n", "android");
      },
      sent: ["a", "android"],
    },
    {
      // A suggestion tap corrects the word in the same breath as adopting it,
      // so the first update carries more than the run. Pins the classifier to
      // a prefix test: an equality test would send the whole word again.
      name: "strips an adopting composition that corrects as it takes over",
      run: (t) => {
        t.type("tes");
        t.composeUpdating("test", "test");
      },
      sent: ["t", "e", "s", "t"],
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
        t.compose("日本");
      },
      sent: ["a", "日本"],
    },
    {
      name: "sends a composition that follows no plain typing",
      run: (t) => t.compose("日本"),
      sent: ["日本"],
    },
    {
      // A composition that stood on its own is not a typed word under the
      // caret, so the next one must reach the pane whole even when it repeats
      // it. Samsung's trace on #3746 composes every word this way.
      name: "sends a character composed twice in a row",
      run: (t) => {
        t.composeUpdating("a", "a");
        t.composeUpdating("a", "a");
      },
      sent: ["a", "a"],
    },
    {
      name: "keeps repeated characters typed without a composition",
      run: (t) => t.type("aa"),
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
  ];

  for (const c of cases) {
    it(c.name, () => {
      const t = renderTerm(c.accepted);
      c.run(t);
      expect(t.sent()).toEqual(c.sent);
    });
  }
});
