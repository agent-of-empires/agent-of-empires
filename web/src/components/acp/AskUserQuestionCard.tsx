// AskUserQuestion / elicitation card. Renders a pending ACP form
// elicitation inline in the conversation, matching ApprovalCard's visual
// language so it reads as part of the same flow.
//
// AskUserQuestion is the common producer (single/multi-select questions
// plus an "Other" free-text box), but the same form path also carries
// arbitrary MCP-server elicitations, so the card renders the full ACP
// form-schema surface: single-select -> radios, multi-select -> checkboxes,
// string -> a text input (typed by `format`), number/integer -> a numeric
// input, boolean -> a checkbox. Submit sends the answers (ACP `accept`);
// Skip sends `decline` (the agent continues with no answer); Cancel sends
// `cancel` (aborts the tool call). Client-side validation mirrors the
// server's (required / length / range / pattern / item bounds), but the
// server re-validates so the browser is never the only gate.

import { useCallback, useMemo, useState } from "react";
import { HelpCircle } from "lucide-react";
import type {
  AnswerValue,
  Elicitation,
  ElicitationOption,
  ElicitationQuestion,
  ElicitationResolution,
} from "../../lib/acpTypes";
import { OFFLINE_TITLE, useServerDown } from "../../lib/connectionState";

interface Props {
  elicitation: Elicitation;
  onResolve: (resolution: ElicitationResolution) => Promise<void>;
}

/** Per-question answer state: a single scalar (string for free-text /
 *  single-select, the numeric input's raw text for number / integer,
 *  "true" / "false" for boolean) plus a set of values for multi-select. */
interface AnswerEntry {
  single: string;
  multi: Set<string>;
}
type AnswerMap = Record<string, AnswerEntry>;

const EMPTY_ENTRY: AnswerEntry = { single: "", multi: new Set<string>() };

/** Definite lookup: every question seeds an entry in `initialAnswers`,
 *  but indexed access is `T | undefined` under noUncheckedIndexedAccess,
 *  so fall back to an empty entry rather than spreading guards. */
function entryFor(answers: AnswerMap, key: string): AnswerEntry {
  return answers[key] ?? EMPTY_ENTRY;
}

const isNumeric = (kind: ElicitationQuestion["kind"]) => kind === "number" || kind === "integer";

/** Seed answer state from each field's `default`, shaped to its kind. */
function initialAnswers(questions: ElicitationQuestion[]): AnswerMap {
  const out: AnswerMap = {};
  for (const q of questions) {
    const entry: AnswerEntry = { single: "", multi: new Set() };
    const d = q.default;
    if (q.kind === "multi_select") {
      if (Array.isArray(d)) entry.multi = new Set(d);
    } else if (q.kind === "boolean") {
      entry.single = d === true ? "true" : "false";
    } else if (isNumeric(q.kind)) {
      if (typeof d === "number") entry.single = String(d);
    } else if (typeof d === "string") {
      entry.single = d;
    }
    out[q.field_key] = entry;
  }
  return out;
}

/** Map a string `format` annotation to a native input type; unknown
 *  formats fall back to plain text. */
function inputTypeFor(format: string | null | undefined): string {
  switch (format) {
    case "email":
      return "email";
    case "uri":
      return "url";
    case "date":
      return "date";
    case "date-time":
      return "datetime-local";
    default:
      return "text";
  }
}

/** The adapter flattens an AskUserQuestion option's `description` into the
 *  enum title as `"<label> — <description>"` (the structured option is
 *  lost on the wire). The bare label survives as the option `value`, so when
 *  the human label is exactly `value` + that separator we can recover the
 *  two-tier label/description; otherwise the title is shown verbatim (a
 *  generic MCP enum where `value` is a code and `label` is the display text). */
const OPTION_DESC_SEP = " — ";
function optionParts(opt: ElicitationOption): { label: string; description?: string } {
  const prefix = `${opt.value}${OPTION_DESC_SEP}`;
  if (opt.label.startsWith(prefix) && opt.label.length > prefix.length) {
    return { label: opt.value, description: opt.label.slice(prefix.length) };
  }
  return { label: opt.label };
}

const labelOf = (q: ElicitationQuestion) => q.title || q.field_key;

/** Pre-submit check for a string `format` annotation. The server treats
 *  format as advisory (the ACP spec says unknown formats are annotations,
 *  not gates), so this only catches obviously malformed email / uri / date
 *  values before a round-trip; it never blocks an unknown format. */
function isValidByFormat(format: string | null | undefined, value: string): boolean {
  switch (format) {
    case "email":
      return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value);
    case "uri":
      try {
        new URL(value);
        return true;
      } catch {
        return false;
      }
    case "date":
      return /^\d{4}-\d{2}-\d{2}$/.test(value);
    case "date-time":
      return !Number.isNaN(Date.parse(value));
    default:
      return true;
  }
}

function validate(questions: ElicitationQuestion[], answers: AnswerMap): string | null {
  for (const q of questions) {
    const a = entryFor(answers, q.field_key);
    const name = labelOf(q);
    if (q.kind === "multi_select") {
      const n = a.multi.size;
      if (q.required && n === 0) return `Please answer: ${name}`;
      if (q.min_items != null && n > 0 && n < q.min_items) return `Select at least ${q.min_items} for ${name}`;
      if (q.max_items != null && n > q.max_items) return `Select at most ${q.max_items} for ${name}`;
    } else if (isNumeric(q.kind)) {
      const v = a.single.trim();
      if (v === "") {
        if (q.required) return `Please answer: ${name}`;
        continue;
      }
      const num = Number(v);
      if (!Number.isFinite(num)) return `Enter a valid number for ${name}`;
      if (q.kind === "integer" && !Number.isInteger(num)) return `${name} must be a whole number`;
      if (q.minimum != null && num < q.minimum) return `${name} must be at least ${q.minimum}`;
      if (q.maximum != null && num > q.maximum) return `${name} must be at most ${q.maximum}`;
    } else if (q.kind === "boolean") {
      // A checkbox always carries a definite value; nothing to validate.
      continue;
    } else {
      // free_text / single_select
      const v = a.single;
      if (q.required && v.trim() === "") return `Please answer: ${name}`;
      if (q.kind === "free_text" && v !== "") {
        if (!isValidByFormat(q.format, v)) return `${name} is not a valid ${q.format}`;
        const len = [...v].length;
        if (q.min_length != null && len < q.min_length) return `${name} must be at least ${q.min_length} characters`;
        if (q.max_length != null && len > q.max_length) return `${name} must be at most ${q.max_length} characters`;
        if (q.pattern) {
          try {
            if (!new RegExp(q.pattern).test(v)) return `${name} does not match the required format`;
          } catch {
            // An unparseable pattern is treated as no constraint, matching
            // the server, which skips invalid regexes.
          }
        }
      }
    }
  }
  return null;
}

function toResolution(questions: ElicitationQuestion[], answers: AnswerMap): ElicitationResolution {
  const payload: Record<string, AnswerValue> = {};
  for (const q of questions) {
    const a = entryFor(answers, q.field_key);
    if (q.kind === "multi_select") {
      if (a.multi.size > 0) payload[q.field_key] = [...a.multi];
    } else if (isNumeric(q.kind)) {
      const v = a.single.trim();
      if (v !== "") payload[q.field_key] = Number(v);
    } else if (q.kind === "boolean") {
      payload[q.field_key] = a.single === "true";
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
      className="my-2 overflow-hidden rounded-md border border-surface-800/60 bg-surface-800/50 text-sm"
      role="alertdialog"
      aria-label="Question from the agent"
    >
      <div className="flex w-full items-center gap-2 border-b border-surface-800/60 px-3 py-2">
        <HelpCircle className="h-3.5 w-3.5 shrink-0 text-brand-500" />
        <span className="shrink-0 text-[11px] uppercase tracking-wider text-brand-500">Question</span>
        {elicitation.title && (
          <span className="min-w-0 flex-1 truncate text-xs text-text-secondary">{elicitation.title}</span>
        )}
      </div>

      <div className="flex flex-col gap-4 px-3 py-3">
        {/* The full prompt wraps here rather than being truncated, so a long
            question is never cut off. */}
        <p className="whitespace-pre-wrap break-words text-xs text-text-secondary">{elicitation.message}</p>
        {elicitation.description && (
          <p className="whitespace-pre-wrap break-words text-[11px] text-text-dim">{elicitation.description}</p>
        )}
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
            phase === "submitting" ? "bg-brand-700 opacity-70 cursor-wait" : "bg-brand-600 hover:bg-brand-500",
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
  const inputClass =
    "w-full rounded-md border border-surface-700 bg-surface-900 px-2 py-1.5 text-xs text-text-primary outline-none focus:border-brand-600 disabled:opacity-60";

  return (
    <fieldset className="min-w-0 border-0 p-0">
      {question.kind !== "boolean" && question.title && (
        <legend className="mb-1 text-xs font-medium text-text-secondary">
          {question.title}
          {question.required && <span className="ml-1 text-rose-400">*</span>}
        </legend>
      )}
      {question.kind !== "boolean" && question.description && (
        <p className="mb-1.5 text-[11px] text-text-dim">{question.description}</p>
      )}

      {question.kind === "boolean" ? (
        <label
          className={[
            "flex cursor-pointer items-center gap-2 rounded-md border px-2 py-1.5 text-xs",
            single === "true"
              ? "border-brand-600 bg-brand-700/15 text-text-primary"
              : "border-surface-700 bg-surface-900 text-text-secondary hover:bg-surface-800",
            disabled ? "cursor-not-allowed opacity-60" : "",
          ].join(" ")}
        >
          <input
            type="checkbox"
            className="accent-brand-600"
            checked={single === "true"}
            disabled={disabled}
            onChange={(e) => onSetSingle(e.target.checked ? "true" : "false")}
          />
          <span className="min-w-0 break-words">
            {question.title || "Yes"}
            {question.required && <span className="ml-1 text-rose-400">*</span>}
          </span>
        </label>
      ) : isNumeric(question.kind) ? (
        <input
          type="number"
          className={inputClass}
          placeholder="Enter a number"
          value={single}
          step={question.kind === "integer" ? "1" : "any"}
          min={question.minimum ?? undefined}
          max={question.maximum ?? undefined}
          disabled={disabled}
          onChange={(e) => onSetSingle(e.target.value)}
        />
      ) : question.kind === "free_text" ? (
        <input
          type={inputTypeFor(question.format)}
          className={inputClass}
          placeholder="Type your answer"
          value={single}
          maxLength={question.max_length ?? undefined}
          disabled={disabled}
          onChange={(e) => onSetSingle(e.target.value)}
        />
      ) : (
        <div className="flex flex-col gap-1">
          {question.options.map((opt) => {
            const isMulti = question.kind === "multi_select";
            const checked = isMulti ? multi.has(opt.value) : single === opt.value;
            const { label, description } = optionParts(opt);
            return (
              <label
                key={opt.value}
                className={[
                  "flex cursor-pointer items-start gap-2 rounded-md border px-2 py-1.5 text-xs",
                  checked
                    ? "border-brand-600 bg-brand-700/15 text-text-primary"
                    : "border-surface-700 bg-surface-900 text-text-secondary hover:bg-surface-800",
                  disabled ? "cursor-not-allowed opacity-60" : "",
                ].join(" ")}
              >
                <input
                  type={isMulti ? "checkbox" : "radio"}
                  name={isMulti ? undefined : groupName}
                  className="mt-0.5 accent-brand-600"
                  checked={checked}
                  disabled={disabled}
                  onChange={() => (isMulti ? onToggleMulti(opt.value) : onSetSingle(opt.value))}
                />
                <span className="min-w-0 break-words">
                  <span className={description ? "font-medium" : undefined}>{label}</span>
                  {description && <span className="block text-[11px] text-text-dim">{description}</span>}
                </span>
              </label>
            );
          })}
        </div>
      )}
    </fieldset>
  );
}
