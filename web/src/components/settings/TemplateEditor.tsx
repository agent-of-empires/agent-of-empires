import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { VARIABLE } from "../../lib/suggestionTemplate";

function highlight(text: string, known: Set<string>) {
  const parts: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(VARIABLE)) {
    const at = m.index ?? 0;
    parts.push(text.slice(last, at));
    parts.push(
      <span
        key={at}
        className={known.has(m[1] ?? "") ? "text-brand-400" : "underline decoration-wavy decoration-status-error"}
      >
        {m[0]}
      </span>,
    );
    last = at + m[0].length;
  }
  parts.push(text.slice(last));
  return parts;
}

interface Props {
  value: string;
  variables: string[];
  disabled: boolean;
  label: string;
  onChange: (value: string) => void;
  onCommit: () => void;
}

export function TemplateEditor({ value, variables, disabled, label, onChange, onCommit }: Props) {
  const area = useRef<HTMLTextAreaElement>(null);
  const caret = useRef<number | null>(null);
  const [showAll, setShowAll] = useState(false);
  const known = new Set(variables);

  useLayoutEffect(() => {
    if (caret.current !== null && area.current) {
      area.current.focus();
      area.current.setSelectionRange(caret.current, caret.current);
      caret.current = null;
    }
  });

  const insert = (name: string) => {
    const el = area.current;
    const start = el?.selectionStart ?? value.length;
    const end = el?.selectionEnd ?? value.length;
    const token = `{${name}}`;
    caret.current = start + token.length;
    onChange(value.slice(0, start) + token + value.slice(end));
  };

  const text = "w-full whitespace-pre-wrap break-words px-3 py-2 font-mono text-xs leading-5";
  return (
    <div>
      <div className="relative rounded-md border border-surface-700 bg-surface-950">
        <div aria-hidden className={`${text} min-h-16 text-text-primary`}>
          {highlight(value, known)}
          {"\n"}
        </div>
        <textarea
          ref={area}
          aria-label={label}
          value={value}
          disabled={disabled}
          spellCheck={false}
          onChange={(e) => onChange(e.target.value)}
          onBlur={onCommit}
          className={`${text} absolute inset-0 h-full resize-none overflow-hidden bg-transparent text-transparent caret-text-primary outline-none`}
        />
      </div>
      <div className="mt-2 flex flex-wrap gap-1.5">
        {variables.map((name, i) => (
          <button
            key={name}
            type="button"
            disabled={disabled}
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => insert(name)}
            className={`h-7 items-center rounded-md border border-surface-700 px-2 font-mono text-[11px] text-text-secondary hover:bg-surface-800 disabled:opacity-50 ${
              i >= 3 && !showAll ? "hidden sm:inline-flex" : "inline-flex"
            }`}
          >
            {`{${name}}`}
          </button>
        ))}
        {variables.length > 3 && !showAll && (
          <button
            type="button"
            onClick={() => setShowAll(true)}
            className="inline-flex h-7 items-center px-1 font-mono text-[11px] text-text-dim sm:hidden"
          >
            +{variables.length - 3}
          </button>
        )}
      </div>
    </div>
  );
}
