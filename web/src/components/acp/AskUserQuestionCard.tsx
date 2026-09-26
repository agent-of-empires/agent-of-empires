// Inline card for a pending ACP form elicitation (AskUserQuestion and MCP
// elicitations). Submit accepts, Skip declines, Cancel aborts the tool call.
// Validation mirrors the server's, which re-validates anyway.

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

/** `single` holds free text, the selected value, raw numeric text, or "true"/"false"; `multi` holds multi-select values. */
interface AnswerEntry {
  single: string;
  multi: Set<string>;
}
type AnswerMap = Record<string, AnswerEntry>;

const EMPTY_ENTRY: AnswerEntry = { single: "", multi: new Set<string>() };

function entryFor(answers: AnswerMap, key: string): AnswerEntry {
  return answers[key] ?? EMPTY_ENTRY;
}

const isNumeric = (kind: ElicitationQuestion["kind"]) => kind === "number" || kind === "integer";

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

const INPUT_TYPES: Record<string, string> = { email: "email", uri: "url", date: "date", "date-time": "datetime-local" };
const inputTypeFor = (format: string | null | undefined) =>
  format && Object.hasOwn(INPUT_TYPES, format) ? INPUT_TYPES[format]! : "text";

/** Older adapters flattened options to `"<value> — <description>"`; recover the two
 *  tiers from that, but a structured `description` (even empty) wins. */
const OPTION_DESC_SEP = " — ";
function optionParts(opt: ElicitationOption): { label: string; description?: string } {
  const prefix = `${opt.value}${OPTION_DESC_SEP}`;
  if (opt.label.startsWith(prefix) && opt.label.length > prefix.length) {
    return { label: opt.value, description: opt.description ?? opt.label.slice(prefix.length) };
  }
  return { label: opt.label, description: opt.description ?? undefined };
}

const labelOf = (q: ElicitationQuestion) => q.title || q.field_key;

/** Catches obviously malformed known formats; unknown formats are advisory and never block. */
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
      continue;
    } else {
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
            // Like the server, an invalid regex is no constraint.
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
    <form
      className="my-2 overflow-hidden rounded-md border border-surface-800/60 bg-surface-800/50 text-sm"
      role="alertdialog"
      aria-label="Question from the agent"
      noValidate
      onSubmit={(e) => {
        e.preventDefault();
        submit();
      }}
    >
      <div className="flex w-full items-center gap-2 border-b border-surface-800/60 px-3 py-2">
        <HelpCircle className="h-3.5 w-3.5 shrink-0 text-brand-500" />
        <span className="shrink-0 text-[11px] uppercase tracking-wider text-brand-500">Question</span>
        {elicitation.title && (
          <span className="min-w-0 flex-1 truncate text-xs text-text-secondary">{elicitation.title}</span>
        )}
      </div>

      <div className="flex flex-col gap-4 px-3 py-3">
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
          type="submit"
          className={[
            "flex flex-1 items-center justify-center gap-1.5 rounded-md py-2 px-3 text-xs font-medium text-white",
            phase === "submitting" ? "bg-brand-700 opacity-70 cursor-wait" : "bg-brand-600 hover:bg-brand-500",
          ].join(" ")}
          disabled={disabled}
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
    </form>
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
      ) : question.kind === "free_text" && !question.format ? (
        <textarea
          className={`${inputClass} resize-y`}
          rows={3}
          placeholder="Type your answer"
          value={single}
          maxLength={question.max_length ?? undefined}
          disabled={disabled}
          onChange={(e) => onSetSingle(e.target.value)}
          onKeyDown={(e) => {
            if (e.key !== "Enter" || e.shiftKey || e.nativeEvent.isComposing) return;
            e.preventDefault();
            e.currentTarget.form?.requestSubmit();
          }}
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
