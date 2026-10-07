import { useRef, useState } from "react";
import { useBranchSuggestions } from "./branchSuggestions";
import { ToggleRow } from "./Toggle";
import type { WizardData } from "../wizardReducer";

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
}

/** Branch name, attach mode and base branch, behind the wizard's Worktree row. */
export function WorktreePanel({ data, onChange }: Props) {
  return (
    <div>
      <label htmlFor="wizard-worktree-branch" className="block text-sm text-text-dim mb-1.5">
        Branch / worktree name
      </label>
      <input
        id="wizard-worktree-branch"
        type="text"
        value={data.worktreeBranch}
        onChange={(e) => onChange("worktreeBranch", e.target.value)}
        placeholder="Uses session title if empty"
        className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-base md:text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
      />
      <p className="text-xs text-text-dim mt-1">The branch name is also the worktree directory name.</p>

      <ToggleRow
        className="mt-4 cursor-pointer"
        title="Attach to existing branch"
        description="Re-use a branch and worktree that already exist. Off creates a new branch."
        checked={data.attachExisting}
        onChange={(v) => onChange("attachExisting", v)}
      />

      {!data.attachExisting && <BaseBranchPicker data={data} onChange={onChange} />}
    </div>
  );
}

/** Base-branch combobox; blank means the repo default. */
function BaseBranchPicker({ data, onChange }: { data: WizardData; onChange: (field: string, value: unknown) => void }) {
  const [highlightIdx, setHighlightIdx] = useState(0);
  const [hasFocus, setHasFocus] = useState(false);
  const blurTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const { loading, suggestions } = useBranchSuggestions(data.path, true, data.baseBranch, 8);

  const choose = (name: string) => {
    onChange("baseBranch", name);
    setHasFocus(false);
  };

  return (
    <div className="mt-4">
      <label className="block text-sm text-text-dim mb-1.5">Base branch</label>
      <div className="relative">
        <input
          type="text"
          value={data.baseBranch}
          onChange={(e) => {
            onChange("baseBranch", e.target.value);
            setHighlightIdx(0);
          }}
          onFocus={() => {
            if (blurTimer.current) clearTimeout(blurTimer.current);
            setHasFocus(true);
          }}
          onBlur={() => {
            blurTimer.current = setTimeout(() => setHasFocus(false), 120);
          }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setHighlightIdx((i) => Math.min(i + 1, suggestions.length - 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setHighlightIdx((i) => Math.max(i - 1, 0));
            } else if (e.key === "Enter" && suggestions[highlightIdx]) {
              e.preventDefault();
              choose(suggestions[highlightIdx].name);
            } else if (e.key === "Escape") {
              setHasFocus(false);
            }
          }}
          placeholder={loading ? "Loading branches..." : "Defaults to project default branch"}
          aria-label="Base branch"
          autoComplete="off"
          className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2 text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
        />
        {hasFocus && suggestions.length > 0 && (
          <ul
            role="listbox"
            aria-label="Branch suggestions"
            className="absolute z-10 left-0 right-0 mt-1 max-h-64 overflow-y-auto bg-surface-900 border border-surface-700/60 rounded-lg shadow-lg"
          >
            {suggestions.map((b, i) => (
              <li
                key={`${b.name}-${b.remote_only ? "r" : "l"}`}
                role="option"
                aria-selected={i === highlightIdx}
                onMouseEnter={() => setHighlightIdx(i)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  choose(b.name);
                }}
                className={`flex items-center justify-between gap-2 px-3 py-1.5 text-sm font-mono cursor-pointer ${
                  i === highlightIdx ? "bg-surface-800 text-text-primary" : "text-text-secondary"
                }`}
              >
                <span className="truncate">{b.name}</span>
                <span className="text-[10px] uppercase tracking-wider text-text-dim shrink-0">
                  {b.is_current ? "current" : b.remote_only ? "remote" : "local"}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
      <p className="text-xs text-text-dim mt-1">
        Stack a new worktree on top of a different branch (an in-flight PR, a release branch, a teammate's branch).
        Leave blank for the repo's default.
      </p>
    </div>
  );
}
