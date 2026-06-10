// AskUserQuestion card. Renders a pending ACP form elicitation inline in
// the conversation, matching ApprovalCard's visual language so it reads as
// part of the same flow.
//
// Single-select questions render as radios, multi-select as checkboxes,
// and a plain string field (the AskUserQuestion "custom answer" box) as a
// text input. Submit sends the chosen labels (ACP `accept`); Skip sends
// `decline` (the agent continues with no answer); Cancel sends `cancel`
// (aborts the tool call). Client-side validation mirrors the server's:
// required questions must be answered and multi-select min/max enforced,
// but the server re-validates so the browser is never the only gate.

import { useCallback, useMemo, useState } from "react";
import { HelpCircle } from "lucide-react";
import type { Elicitation, ElicitationQuestion, ElicitationResolution } from "../../lib/acpTypes";
import { OFFLINE_TITLE, useServerDown } from "../../lib/connectionState";

interface Props {
  elicitation: Elicitation;
  onResolve: (resolution: ElicitationResolution) => Promise<void>;
}

/** Per-question answer state: a single value for free-text / single-select,
 *  a set of values for multi-select. */
interface AnswerEntry {
  single: string;
  multi: Set<string>;
}
type AnswerMap = Record<string, AnswerEntry>;

/** Definite lookup: every question seeds an entry in `initialAnswers`,
 *  but indexed access is `T | undefined` under noUncheckedIndexedAccess,
 *  so fall back to an empty entry rather than spreading guards. */
function entryFor(answers: AnswerMap, key: string): AnswerEntry {
  return answers[key] ?? { single: "", multi: new Set<string>() };
}

function initialAnswers(questions: ElicitationQuestion[]): AnswerMap {
  const out: AnswerMap = {};
  for (const q of questions) {
    out[q.field_key] = { single: "", multi: new Set() };
  }
  return out;
}

function validate(questions: ElicitationQuestion[], answers: AnswerMap): string | null {
  for (const q of questions) {
    const a = entryFor(answers, q.field_key);
    if (q.kind === "multi_select") {
      const n = a.multi.size;
      if (q.required && n === 0) return `Please answer: ${q.title || q.field_key}`;
      if (q.min_items != null && n < q.min_items) return `Select at least ${q.min_items} for ${q.title || q.field_key}`;
      if (q.max_items != null && n > q.max_items) return `Select at most ${q.max_items} for ${q.title || q.field_key}`;
    } else if (q.required && a.single.trim() === "") {
      return `Please answer: ${q.title || q.field_key}`;
    }
  }
  return null;
}

function toResolution(questions: ElicitationQuestion[], answers: AnswerMap): ElicitationResolution {
  const payload: Record<string, string | string[]> = {};
  for (const q of questions) {
    const a = entryFor(answers, q.field_key);
    if (q.kind === "multi_select") {
      if (a.multi.size > 0) payload[q.field_key] = [...a.multi];
    } else if (a.single.trim() !== "") {
      payload[q.field_key] = a.single;
    }
  }
  return { action: "accept", answers: payload };
}

export function AskUserQuestionCard({ elicitation, onResolve }: Props) {
  const offline = useServerDown();
  const [phase, setPhase] = useState<"pending" | "submitting" | "rolled-back">("pending");
  const [answers, setAnswers] = useState<AnswerMap>(() => initialAnswers(elicitation.questions));
  const [error, setError] = useState<string | null>(null);

  const single = elicitation.questions.length === 1;

  const setSingle = useCallback((field: string, value: string) => {
    setAnswers((prev) => ({ ...prev, [field]: { ...entryFor(prev, field), single: value } }));
  }, []);

  const toggleMulti = useCallback((field: string, value: string) => {
    setAnswers((prev) => {
      const prevEntry = entryFor(prev, field);
      const multi = new Set(prevEntry.multi);
      if (multi.has(value)) multi.delete(value);
      else multi.add(value);
      return { ...prev, [field]: { ...prevEntry, multi } };
    });
  }, []);

  const run = useCallback(
    async (resolution: ElicitationResolution) => {
      setPhase("submitting");
      try {
        await onResolve(resolution);
      } catch {
        setPhase("rolled-back");
      }
    },
    [onResolve],
  );

  const submit = useCallback(() => {
    const msg = validate(elicitation.questions, answers);
    if (msg) {
      setError(msg);
      return;
    }
    setError(null);
    void run(toResolution(elicitation.questions, answers));
  }, [elicitation.questions, answers, run]);

  const disabled = offline || phase === "submitting";

  return (
    <div
      className="my-2 overflow-hidden rounded-md border border-brand-700/40 bg-brand-700/5 text-sm"
      role="alertdialog"
      aria-label="Question from the agent"
    >
      <div className="flex w-full items-center gap-2 border-b border-surface-800/60 px-3 py-2">
        <HelpCircle className="h-3.5 w-3.5 shrink-0 text-brand-500" />
        <span className="shrink-0 text-[11px] uppercase tracking-wider text-brand-500">Question</span>
        {single && <span className="min-w-0 flex-1 truncate text-xs text-text-secondary">{elicitation.message}</span>}
      </div>

      <div className="flex flex-col gap-4 px-3 py-3">
        {!single && <p className="text-xs text-text-secondary">{elicitation.message}</p>}
        {elicitation.questions.map((q) => (
          <QuestionField
            key={q.field_key}
            question={q}
            single={entryFor(answers, q.field_key).single}
            multi={entryFor(answers, q.field_key).multi}
            disabled={disabled}
            onSetSingle={(v) => setSingle(q.field_key, v)}
            onToggleMulti={(v) => toggleMulti(q.field_key, v)}
          />
        ))}
      </div>

      {error && <p className="px-3 pb-1 text-xs text-rose-400">{error}</p>}
      {phase === "rolled-back" && (
        <p className="px-3 pb-1 text-xs text-rose-400">Could not reach the server. Try again.</p>
      )}
      {offline && <p className="px-3 pb-1 text-xs text-status-error">{OFFLINE_TITLE}</p>}

      <div className="flex items-stretch gap-1.5 border-t border-surface-800/60 p-2">
        <button
          type="button"
          className={[
            "flex flex-1 items-center justify-center gap-1.5 rounded-md py-2 px-3 text-xs font-medium text-white",
            phase === "pending" ? "bg-brand-600 hover:bg-brand-500" : "bg-brand-700 opacity-70 cursor-wait",
          ].join(" ")}
          disabled={disabled}
          onClick={submit}
        >
          {phase === "submitting" ? "Submitting…" : "Submit"}
        </button>
        <button
          type="button"
          className="flex items-center justify-center rounded-md border border-surface-700 bg-surface-800 py-2 px-3 text-xs font-medium text-text-secondary hover:bg-surface-700 disabled:opacity-60"
          disabled={disabled}
          onClick={() => void run({ action: "decline" })}
          title="Skip this question; the agent continues without an answer"
        >
          Skip
        </button>
        <button
          type="button"
          className="flex items-center justify-center rounded-md border border-surface-700 bg-surface-800 py-2 px-3 text-xs font-medium text-text-secondary hover:border-rose-700/60 hover:bg-rose-950/30 hover:text-rose-300 disabled:opacity-60"
          disabled={disabled}
          onClick={() => void run({ action: "cancel" })}
          title="Cancel the agent's tool call"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

function QuestionField({
  question,
  single,
  multi,
  disabled,
  onSetSingle,
  onToggleMulti,
}: {
  question: ElicitationQuestion;
  single: string;
  multi: Set<string>;
  disabled: boolean;
  onSetSingle: (value: string) => void;
  onToggleMulti: (value: string) => void;
}) {
  // A radio group needs a stable per-question name so selections don't
  // bleed across questions in a multi-question form.
  const groupName = useMemo(() => `elicit-${question.field_key}`, [question.field_key]);

  return (
    <fieldset className="min-w-0 border-0 p-0">
      {question.title && (
        <legend className="mb-1 text-xs font-medium text-text-secondary">
          {question.title}
          {question.required && <span className="ml-1 text-rose-400">*</span>}
        </legend>
      )}
      {question.description && <p className="mb-1.5 text-[11px] text-text-dim">{question.description}</p>}

      {question.kind === "free_text" ? (
        <input
          type="text"
          className="w-full rounded-md border border-surface-700 bg-surface-900 px-2 py-1.5 text-xs text-text-primary outline-none focus:border-brand-600 disabled:opacity-60"
          placeholder="Type your answer"
          value={single}
          disabled={disabled}
          onChange={(e) => onSetSingle(e.target.value)}
        />
      ) : (
        <div className="flex flex-col gap-1">
          {question.options.map((opt) => {
            const isMulti = question.kind === "multi_select";
            const checked = isMulti ? multi.has(opt.value) : single === opt.value;
            return (
              <label
                key={opt.value}
                className={[
                  "flex cursor-pointer items-center gap-2 rounded-md border px-2 py-1.5 text-xs",
                  checked
                    ? "border-brand-600 bg-brand-700/15 text-text-primary"
                    : "border-surface-700 bg-surface-900 text-text-secondary hover:bg-surface-800",
                  disabled ? "cursor-not-allowed opacity-60" : "",
                ].join(" ")}
              >
                <input
                  type={isMulti ? "checkbox" : "radio"}
                  name={isMulti ? undefined : groupName}
                  className="accent-brand-600"
                  checked={checked}
                  disabled={disabled}
                  onChange={() => (isMulti ? onToggleMulti(opt.value) : onSetSingle(opt.value))}
                />
                <span className="min-w-0 break-words">{opt.label}</span>
              </label>
            );
          })}
        </div>
      )}
    </fieldset>
  );
}
