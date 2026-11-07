// Rendered markdown that reveals a long text in steps and says how much is hidden.

import { TextMessagePartProvider } from "@assistant-ui/react";
import { useState } from "react";

import { Markdown } from "../Markdown";
import { previewParagraphs, revealLines, STEP_LINES } from "./longText";

function Rendered({ text }: { text: string }) {
  return (
    <TextMessagePartProvider text={text}>
      <Markdown text={text} />
    </TextMessagePartProvider>
  );
}

const MORE_BUTTON = "mt-1 text-[11px] text-accent-600 hover:underline";

/** A report: 40 lines at a time. */
export function SteppedMarkdown({ text }: { text: string }) {
  const [limit, setLimit] = useState(STEP_LINES);
  const { text: shown, remaining } = revealLines(text, limit);
  return (
    <>
      <Rendered text={shown} />
      {remaining > 0 && (
        <button type="button" className={MORE_BUTTON} onClick={() => setLimit((n) => n + STEP_LINES)}>
          Show {Math.min(STEP_LINES, remaining)} more lines, {remaining} remaining
        </button>
      )}
    </>
  );
}

/** A prompt: a short preview that opens to the whole text. */
export function PreviewMarkdown({ text, lines, noun }: { text: string; lines: number; noun: string }) {
  const [full, setFull] = useState(false);
  const { text: preview, remaining } = previewParagraphs(text, lines);
  if (remaining === 0) return <Rendered text={text} />;
  return (
    <>
      <Rendered text={full ? text : preview} />
      <button type="button" className={MORE_BUTTON} onClick={() => setFull((v) => !v)}>
        {full ? `Collapse ${noun}` : `Show full ${noun}, ${remaining} more ${remaining === 1 ? "line" : "lines"}`}
      </button>
    </>
  );
}
