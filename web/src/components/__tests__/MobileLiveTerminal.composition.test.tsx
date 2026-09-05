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
  /** A composition that hands back `data` when it ends. */
  compose: (data: string) => void;
  input: (inputType: string) => void;
  sent: () => string[];
}

function renderTerm(): Term {
  const inputRef = createRef<HTMLTextAreaElement>();
  const sendData = vi.fn();
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
      fireEvent.compositionEnd(input, { data });
    },
    input: (inputType) => beforeInput(inputType, null),
    sent: () => sendData.mock.calls.map(([d]: [string]) => d),
  };
}

describe("MobileLiveTerminal Android IME word commits", () => {
  const cases: { name: string; run: (t: Term) => void; sent: string[] }[] = [
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
      const t = renderTerm();
      c.run(t);
      expect(t.sent()).toEqual(c.sent);
    });
  }
});
