import { useEffect, useRef, useState } from "react";
import { RotateCcw } from "lucide-react";
import { fetchBranches, setSessionDiffBase, type BranchInfo } from "../../lib/api";
import { isImeComposing } from "../../lib/ime";
import { INPUT, MENU, MENU_ITEM } from "../../lib/surfaces";
import { MenuItem } from "../ContextMenu";

const OPTION = MENU_ITEM.replace("text-[13px]", "font-mono text-[12px]");
const OPTION_HIGHLIGHTED = `${OPTION.replace("text-text-secondary", "text-text-primary")} bg-surface-700/60`;

interface BasePickerProps {
  sessionId: string;
  repoPath: string;
  currentBase: string;
  hasOverride: boolean;
  /** Omitted for a single-repo session. */
  repoName?: string;
  onChanged?: () => void;
}

/** The `vs <ref>` chip with a branch typeahead that sets one repo's diff base. */
export function BasePicker({ sessionId, repoPath, currentBase, hasOverride, repoName, onChanged }: BasePickerProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [branches, setBranches] = useState<BranchInfo[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [highlightIdx, setHighlightIdx] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    fetchBranches(repoPath, true).then((rows) => {
      if (!cancelled) setBranches(rows ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [open, repoPath]);

  useEffect(() => {
    if (!open) return;
    const onDocPointer = (e: PointerEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("pointerdown", onDocPointer);
    return () => document.removeEventListener("pointerdown", onDocPointer);
  }, [open]);

  const q = query.trim().toLowerCase();
  const suggestions = (branches ?? []).filter((b) => !q || b.name.toLowerCase().includes(q)).slice(0, 8);

  const apply = async (value: string | null) => {
    setBusy(true);
    const ok = await setSessionDiffBase(sessionId, value, repoName);
    setBusy(false);
    if (ok) {
      setOpen(false);
      setQuery("");
      onChanged?.();
    }
  };

  return (
    <div ref={containerRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-label={`Change diff base (current: ${currentBase})`}
        title="Change diff base"
        className={`font-mono text-[10px] px-1.5 py-px rounded cursor-pointer transition-colors ${
          hasOverride
            ? "bg-brand-600/15 text-brand-500 hover:bg-brand-600/25"
            : "bg-surface-800 text-text-muted hover:bg-surface-700"
        }`}
      >
        vs {currentBase}
      </button>
      {open && (
        <div className={`absolute left-0 top-full z-20 mt-1 w-72 ${MENU}`}>
          <div className="px-2 pt-1 pb-1.5">
            <input
              type="text"
              autoFocus
              value={query}
              placeholder="Search branches..."
              onChange={(e) => {
                setQuery(e.target.value);
                setHighlightIdx(0);
              }}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setHighlightIdx((i) => Math.min(i + 1, suggestions.length - 1));
                } else if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setHighlightIdx((i) => Math.max(i - 1, 0));
                } else if (e.key === "Enter" && !isImeComposing(e)) {
                  e.preventDefault();
                  const pick = suggestions[highlightIdx];
                  if (pick) void apply(pick.name);
                  else if (query.trim()) void apply(query.trim());
                } else if (e.key === "Escape") {
                  setOpen(false);
                }
              }}
              disabled={busy}
              className={`${INPUT} font-mono`}
            />
          </div>
          {hasOverride && (
            <MenuItem onClick={() => void apply(null)} disabled={busy} icon={<RotateCcw />}>
              Reset to auto-detected
            </MenuItem>
          )}
          <ul role="listbox" aria-label="Branch suggestions" className="max-h-56 overflow-y-auto">
            {suggestions.length === 0 && (
              <li className="px-3 py-1.5 text-[12px] text-text-dim">
                {branches === null ? "Loading branches..." : "No matches."}
              </li>
            )}
            {suggestions.map((b, i) => (
              <li
                key={`${b.name}-${b.remote_only ? "r" : "l"}`}
                role="option"
                aria-selected={i === highlightIdx}
                onMouseEnter={() => setHighlightIdx(i)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  void apply(b.name);
                }}
                className={i === highlightIdx ? OPTION_HIGHLIGHTED : OPTION}
              >
                <span className="min-w-0 truncate">{b.name}</span>
                <span className="ml-auto shrink-0 text-[11px] text-text-dim">
                  {b.is_current ? "current" : b.remote_only ? "remote" : "local"}
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
