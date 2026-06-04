import { useEffect, useRef, useState } from "react";
import { fetchBranches } from "../../../lib/api";
import type { BranchInfo } from "../../../lib/api";

interface WizardData {
  path: string;
  title: string;
  worktreeBranch: string;
  useWorktree: boolean;
  /** Attach to an existing branch's worktree instead of creating one.
   *  Mirrors the TUI new-session toggle. See #969. */
  attachExisting: boolean;
  baseBranch: string;
  group: string;
  tool: string;
  scratch: boolean;
  [key: string]: unknown;
}

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
}

function Toggle({ checked, onChange, disabled }: { checked: boolean; onChange: (v: boolean) => void; disabled?: boolean }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      onClick={() => !disabled && onChange(!checked)}
      className={`relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors duration-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
        disabled ? "opacity-40 cursor-not-allowed" : "cursor-pointer"
      } ${checked ? "bg-brand-600" : "bg-surface-700"}`}
    >
      <span
        className={`inline-block h-5 w-5 rounded-full bg-white shadow-sm transition-transform duration-200 ${
          checked ? "translate-x-6" : "translate-x-1"
        }`}
      />
    </button>
  );
}

export function SessionStep({ data, onChange }: Props) {
  // Everything except the title input is folded behind this disclosure
  // so the common "name it, hit Next" path is a single visible control.
  // Defaults to a new worktree branched off HEAD; the submit path in
  // SessionWizard reads useWorktree/worktreeBranch/attachExisting/
  // baseBranch/group from reducer state regardless of what is on screen,
  // so a hidden toggle keeps its default value. See #1514.
  const [advancedOpen, setAdvancedOpen] = useState(false);
  return (
    <div>
      <h2 className="text-lg font-semibold text-text-primary mb-1">Name your session</h2>
      <p className="text-sm text-text-muted mb-5">
        Give it a title; defaults to a new git worktree branched off the current HEAD. Expand Advanced to change the branch, attach to an existing one, or set a group.
      </p>

      <div className="mb-5">
        <label className="block text-sm text-text-dim mb-1.5">Session title</label>
        <input
          type="text"
          value={data.title}
          onChange={(e) => onChange("title", e.target.value)}
          placeholder="Auto-generated if empty"
          className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-base font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
        />
        <p className="text-xs text-text-dim mt-1">Shown in the dashboard. Renaming it later does not rename the git branch.</p>
      </div>

      <button
        type="button"
        onClick={() => setAdvancedOpen((v) => !v)}
        aria-expanded={advancedOpen}
        className="flex items-center gap-1.5 text-sm text-text-dim hover:text-text-secondary cursor-pointer"
      >
        <span
          className={`inline-block transition-transform ${advancedOpen ? "rotate-90" : ""}`}
          aria-hidden="true"
        >
          ▸
        </span>
        Advanced
      </button>
      {advancedOpen && (
        <div className="mt-3 pl-4 border-l border-surface-700/40">
          {/* Worktree controls are meaningless for scratch sessions: the
              working directory is a fresh scratch dir, not a git repo. The
              reducer also forces useWorktree to false when scratch flips
              on; this hide is purely a UX confirmation that the worktree
              path is not available in scratch mode. */}
          {data.scratch ? (
            <p
              className="text-xs text-text-dim mb-3"
              aria-label="Worktree disabled: scratch session"
            >
              Scratch sessions do not use git worktrees.
            </p>
          ) : (
            <label
              className="flex items-center justify-between gap-3 p-3 bg-surface-900 border border-surface-700 rounded-lg cursor-pointer mb-3"
              onClick={(e) => {
                // Clicks that land on the Toggle button already drive
                // `onChange("useWorktree", v)`. Letting the label's own
                // handler also fire would flip the value a second time
                // and land back on the original. Skip the label handler
                // for clicks originating inside the Toggle.
                if (
                  (e.target as HTMLElement).closest('button[role="switch"]')
                ) {
                  return;
                }
                onChange("useWorktree", !data.useWorktree);
              }}
            >
              <div className="flex-1">
                <div className="text-sm font-medium text-text-primary">Create a worktree</div>
                <div className="text-xs text-text-dim mt-0.5 leading-snug">
                  Run the agent in a new git worktree branched off the current HEAD. Off = run directly in the repo folder.
                </div>
              </div>
              <Toggle
                checked={data.useWorktree}
                onChange={(v) => onChange("useWorktree", v)}
              />
            </label>
          )}

          {!data.scratch && data.useWorktree && (
            <div className="mb-5">
              <label className="block text-sm text-text-dim mb-1.5">Branch / worktree name</label>
              <input
                type="text"
                value={data.worktreeBranch}
                onChange={(e) => onChange("worktreeBranch", e.target.value)}
                placeholder="Uses session title if empty"
                className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-base font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
              />
              <p className="text-xs text-text-dim mt-1">The branch name is also the worktree directory name. Leave blank to use the session title.</p>

              <label
                className="mt-3 flex items-center justify-between gap-3 p-3 bg-surface-900 border border-surface-700 rounded-lg cursor-pointer"
                onClick={() => onChange("attachExisting", !data.attachExisting)}
              >
                <div className="flex-1">
                  <div className="text-sm font-medium text-text-primary">Attach to existing branch</div>
                  <div className="text-xs text-text-dim mt-0.5 leading-snug">
                    Re-use a branch + worktree that already exists. Off = create a new branch.
                  </div>
                </div>
                <Toggle
                  checked={data.attachExisting}
                  onChange={(v) => onChange("attachExisting", v)}
                />
              </label>

              {!data.attachExisting && (
                <AdvancedWorktreeOptions data={data} onChange={onChange} />
              )}
            </div>
          )}

          <div>
            <label className="block text-sm text-text-dim mb-1.5">Group</label>
            <input
              type="text"
              value={data.group}
              onChange={(e) => onChange("group", e.target.value)}
              placeholder="Optional, for organizing related sessions"
              className="w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
            />
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Collapsed "Base branch" section under the worktree name input. Houses
 * the base-branch picker (#948). Hidden by default and only matters when
 * the user expands it; defaulting to the repo default means the common
 * case stays exactly as before. Labeled "Base branch" rather than
 * "Advanced" since #1514 wraps the whole worktree block in an outer
 * "Advanced" disclosure, and two nested buttons both reading "Advanced"
 * would be ambiguous.
 */
function AdvancedWorktreeOptions({
  data,
  onChange,
}: {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
}) {
  const [open, setOpen] = useState(false);
  const [branches, setBranches] = useState<BranchInfo[] | null>(null);
  const [highlightIdx, setHighlightIdx] = useState(0);
  const [hasFocus, setHasFocus] = useState(false);
  const blurTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const branchLoadKey = open ? data.path : null;
  const [trackedBranchLoadKey, setTrackedBranchLoadKey] = useState(branchLoadKey);
  if (branchLoadKey !== trackedBranchLoadKey) {
    setTrackedBranchLoadKey(branchLoadKey);
    setBranches(null);
    setHighlightIdx(0);
  }
  const loading = open && branches === null;

  useEffect(() => {
    if (!open || !data.path) return;
    let cancelled = false;
    fetchBranches(data.path, true).then((rows) => {
      if (!cancelled) {
        setBranches(rows ?? []);
      }
    });
    return () => {
      cancelled = true;
    };
  }, [open, data.path]);

  const query = data.baseBranch.trim().toLowerCase();
  const suggestions = (branches ?? [])
    .filter((b) => !query || b.name.toLowerCase().includes(query))
    .slice(0, 8);

  const choose = (name: string) => {
    onChange("baseBranch", name);
    setHasFocus(false);
  };

  return (
    <div className="mt-3">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex items-center gap-1.5 text-xs text-text-dim hover:text-text-secondary cursor-pointer"
      >
        <span
          className={`inline-block transition-transform ${open ? "rotate-90" : ""}`}
          aria-hidden="true"
        >
          ▸
        </span>
        Base branch
      </button>
      {open && (
        <div className="mt-2 pl-4 border-l border-surface-700/40">
          <label className="block text-xs text-text-dim mb-1.5">Base branch</label>
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
                      i === highlightIdx
                        ? "bg-surface-800 text-text-primary"
                        : "text-text-secondary"
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
            Stack a new worktree on top of a different branch (an in-flight PR, a release branch, a teammate's branch). Leave blank for the repo's default.
          </p>
        </div>
      )}
    </div>
  );
}
