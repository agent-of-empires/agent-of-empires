import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  findMatches,
  type FindMatch,
  type SearchableLine,
} from "./findMatches";

interface Props {
  /** Lines that find may match against: the diff's changed lines. */
  lines: SearchableLine[];
  /** Called with the active match (or null when none) so the host can
   *  scroll/select it in the virtualized renderer. */
  onJump: (match: FindMatch | null) => void;
  onClose: () => void;
}

/**
 * In-diff find bar. Searches the diff *model* via {@link findMatches} (not the
 * DOM), so it reaches lines the virtualized renderer hasn't mounted. Enter /
 * Shift+Enter step through matches; Esc closes.
 */
export function FindBar({ lines, onJump, onClose }: Props) {
  const [query, setQuery] = useState("");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [regex, setRegex] = useState(false);
  const [active, setActive] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const safeFind = useCallback(
    (q: string, cs: boolean, rx: boolean) => {
      try {
        return {
          matches: findMatches(lines, q, { caseSensitive: cs, regex: rx }),
          error: null as string | null,
        };
      } catch {
        return { matches: [] as FindMatch[], error: "Invalid pattern" };
      }
    },
    [lines],
  );

  const { matches, error } = useMemo(
    () => safeFind(query, caseSensitive, regex),
    [safeFind, query, caseSensitive, regex],
  );

  // Displayed index is derived at render; jumps fire from the event handlers
  // below (not an effect), per the no-set-state-in-effect lint posture.
  const activeIdx =
    matches.length === 0 ? 0 : Math.min(active, matches.length - 1);

  /** Re-run the search with new inputs and jump to its first match. */
  const retarget = (q: string, cs: boolean, rx: boolean) => {
    setActive(0);
    onJump(safeFind(q, cs, rx).matches[0] ?? null);
  };

  const step = (delta: number) => {
    if (matches.length === 0) return;
    const next = (activeIdx + delta + matches.length) % matches.length;
    setActive(next);
    onJump(matches[next] ?? null);
  };

  return (
    <div className="flex items-center gap-1 px-3 py-1.5 border-b border-surface-700/20 bg-surface-850 shrink-0">
      <input
        ref={inputRef}
        type="text"
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          retarget(e.target.value, caseSensitive, regex);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            step(e.shiftKey ? -1 : 1);
          } else if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          }
        }}
        placeholder="Find in diff"
        aria-label="Find in diff"
        className="flex-1 min-w-0 bg-surface-900 border border-surface-700/40 rounded px-2 py-0.5 text-[12px] font-mono text-text-primary outline-none focus:border-brand-600"
      />
      <span
        className={`font-mono text-[11px] tabular-nums ${error ? "text-status-error" : "text-text-dim"}`}
      >
        {error
          ? error
          : matches.length === 0
            ? query
              ? "0/0"
              : ""
            : `${activeIdx + 1}/${matches.length}`}
      </span>
      <ToggleButton
        active={caseSensitive}
        onClick={() => {
          setCaseSensitive(!caseSensitive);
          retarget(query, !caseSensitive, regex);
        }}
        title="Match case"
        label="Aa"
      />
      <ToggleButton
        active={regex}
        onClick={() => {
          setRegex(!regex);
          retarget(query, caseSensitive, !regex);
        }}
        title="Regular expression"
        label=".*"
      />
      <IconButton
        onClick={() => step(-1)}
        title="Previous match (Shift+Enter)"
        disabled={matches.length === 0}
        label="↑"
      />
      <IconButton
        onClick={() => step(1)}
        title="Next match (Enter)"
        disabled={matches.length === 0}
        label="↓"
      />
      <IconButton onClick={onClose} title="Close (Esc)" label="✕" />
    </div>
  );
}

function ToggleButton({
  active,
  onClick,
  title,
  label,
}: {
  active: boolean;
  onClick: () => void;
  title: string;
  label: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      aria-label={title}
      title={title}
      className={`px-1.5 py-0.5 rounded text-[11px] font-mono cursor-pointer transition-colors ${
        active
          ? "bg-brand-600 text-white"
          : "text-text-dim hover:text-text-secondary"
      }`}
    >
      {label}
    </button>
  );
}

function IconButton({
  onClick,
  title,
  label,
  disabled,
}: {
  onClick: () => void;
  title: string;
  label: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      disabled={disabled}
      aria-label={title}
      className="px-1.5 py-0.5 rounded text-[11px] font-mono text-text-dim hover:text-text-secondary cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-default"
    >
      {label}
    </button>
  );
}
