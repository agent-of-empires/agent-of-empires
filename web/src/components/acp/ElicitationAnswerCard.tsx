import type { ElicitationAnswer } from "../../lib/acpTypes";

interface Props {
  answers: ElicitationAnswer[];
}

/** Rich rendering of the user's reply to an AskUserQuestion / elicitation
 *  form in the structured view user-message slot. Built from the typed
 *  answers carried on the assistant-ui message metadata; falls back to raw
 *  text rendering upstream when absent. Mirrors `DiffCommentsUserCard` so a
 *  structured pick reads as a tidy card, not a flat "Q: A" line. See #2209. */
export function ElicitationAnswerCard({ answers }: Props) {
  return (
    <div
      data-testid="elicitation-answer-card"
      className="w-full max-w-3xl rounded-2xl rounded-br-sm border border-surface-700 bg-surface-800/70 px-4 py-3 text-sm"
    >
      <div className="mb-2 flex items-center gap-2 text-[11px] uppercase tracking-wider text-text-dim">
        <span className="rounded bg-brand-600/15 px-1.5 py-0.5 font-mono text-brand-300">answer</span>
        <span>
          {answers.length} answer{answers.length === 1 ? "" : "s"}
        </span>
      </div>
      <ul className="flex flex-col gap-2">
        {answers.map((a, i) => (
          <li
            key={`${i}-${a.question}`}
            className="rounded-lg border border-surface-700/60 bg-surface-900/60 px-3 py-2"
          >
            <div className="mb-0.5 text-[11px] uppercase tracking-wider text-text-dim">{a.question}</div>
            <div className="whitespace-pre-wrap text-text-primary">{a.answer}</div>
          </li>
        ))}
      </ul>
    </div>
  );
}
