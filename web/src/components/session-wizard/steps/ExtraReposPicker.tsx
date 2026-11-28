import { useEffect, useRef, useState } from "react";
import { fetchBranches, type BranchInfo } from "../../../lib/api";
import { hasFinePointer } from "../../../lib/platform";
import { ProjectSearchList } from "./ProjectSearchList";
import { useProjectPicker } from "./projectPicker";
import { isImeComposing } from "../../../lib/ime";
import { BUTTON_SECONDARY, INPUT } from "../../../lib/surfaces";
import { LISTBOX, LISTBOX_OPTION } from "../WizardRows";
import { FIELD_HINT } from "../../../lib/surfaces";

interface Props {
  profile: string | undefined;
  primaryPath: string;
  selectedPaths: string[];
  onChange: (paths: string[]) => void;
  /** Base branch per repo path. Empty or missing means the repo falls back to
   *  the session-wide base branch. See #3329. */
  repoBases: Record<string, string>;
  onRepoBasesChange: (bases: Record<string, string>) => void;
  /** False while attaching to an existing branch, when no branch is created
   *  and a base has nothing to apply to. */
  basesEnabled: boolean;
}

/// Base-branch typeahead for one extra repo. The branch list comes from that
/// repo's own path, so a workspace member offers its own epic branches rather
/// than the launch repo's. Free text is accepted: a ref that exists only on a
/// remote still resolves at worktree creation. See #3329.
function RepoBaseInput({
  repoPath,
  label,
  value,
  onChange,
}: {
  repoPath: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
}) {
  const [branches, setBranches] = useState<BranchInfo[] | null>(null);
  const [focused, setFocused] = useState(false);

  useEffect(() => {
    if (!focused || branches !== null) return;
    let cancelled = false;
    fetchBranches(repoPath, true).then((rows) => {
      if (!cancelled) setBranches(rows ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [focused, branches, repoPath]);

  const query = value.trim().toLowerCase();
  const suggestions = (branches ?? []).filter((b) => !query || b.name.toLowerCase().includes(query)).slice(0, 6);

  return (
    <div className="relative flex-1 min-w-0">
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onFocus={() => setFocused(true)}
        onBlur={() =>
          setTimeout(() => {
            setFocused(false);
            // Drop the list so re-opening the field refetches: a list that
            // failed or predates a fetch on the remote must not be reused.
            setBranches(null);
          }, 120)
        }
        placeholder="base branch (optional)"
        aria-label={`Base branch for ${label}`}
        autoComplete="off"
        className={`${INPUT} font-mono`}
      />
      {focused && suggestions.length > 0 && (
        <ul
          role="listbox"
          aria-label={`Branch suggestions for ${label}`}
          className={`${LISTBOX} absolute left-0 right-0 top-full z-20 mt-1 max-h-48`}
        >
          {suggestions.map((b) => (
            <li
              key={`${b.name}-${b.remote_only ? "r" : "l"}`}
              role="option"
              aria-selected={b.name === value}
              onMouseDown={(e) => {
                e.preventDefault();
                onChange(b.name);
                setFocused(false);
              }}
              className={LISTBOX_OPTION}
            >
              {b.name}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function ExtraReposPicker({
  profile,
  primaryPath,
  selectedPaths,
  onChange,
  repoBases,
  onRepoBasesChange,
  basesEnabled,
}: Props) {
  const [freeText, setFreeText] = useState("");

  // Hide the primary repo from the picker so users can't accidentally
  // duplicate it (the builder rejects duplicate repo names).
  const { loading, saved, recent, query, setQuery, filteredSaved, filteredRecent, hasAnyProjects } = useProjectPicker(
    profile,
    [primaryPath],
  );

  const pathRef = useRef<HTMLInputElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const opener = useRef(document.activeElement);
  // The search box only exists once projects load, so pick the target then, unless the user moved focus since opening.
  useEffect(() => {
    if (loading || !hasFinePointer()) return;
    const active = document.activeElement;
    if (active && active !== document.body && active !== opener.current) return;
    (searchRef.current ?? pathRef.current)?.focus();
  }, [loading]);

  const setRepoBase = (path: string, base: string) => {
    const next = { ...repoBases };
    if (base.trim()) next[path] = base;
    else delete next[path];
    onRepoBasesChange(next);
  };

  const isSelected = (path: string) => selectedPaths.includes(path);

  const toggle = (path: string) => {
    if (isSelected(path)) {
      onChange(selectedPaths.filter((p) => p !== path));
    } else {
      onChange([...selectedPaths, path]);
    }
  };

  const addFreeText = () => {
    const trimmed = freeText.trim();
    if (!trimmed) return;
    if (selectedPaths.includes(trimmed) || trimmed === primaryPath) {
      setFreeText("");
      return;
    }
    onChange([...selectedPaths, trimmed]);
    setFreeText("");
  };

  const removePath = (path: string) => {
    onChange(selectedPaths.filter((p) => p !== path));
    setRepoBase(path, "");
  };

  return (
    <div data-testid="extra-repos-picker">
      <p className={`${FIELD_HINT} mb-3`}>
        Include additional repositories in the same workspace. Each gets its own worktree on the same branch, forked
        from the session's base branch unless you give it one of its own.{" "}
        <span className="text-text-secondary">
          {selectedPaths.length > 0 ? `${selectedPaths.length} selected` : "none"}
        </span>
      </p>

      {selectedPaths.length > 0 && (
        <div className="flex flex-col gap-1.5 mb-3">
          {selectedPaths.map((path) => {
            const known = saved.find((p) => p.path === path);
            const recentMatch = recent.find((r) => r.path === path);
            const label = known?.name || recentMatch?.displayName || path.split("/").filter(Boolean).pop() || path;
            return (
              <div key={path} className="flex items-center gap-2">
                <span
                  className="inline-flex items-center gap-1 pl-2.5 bg-brand-600/15 border border-brand-600/40 rounded-lg text-[13px] text-text-primary shrink-0"
                  title={path}
                >
                  <span className="font-mono">{label}</span>
                  <button
                    type="button"
                    onClick={() => removePath(path)}
                    className="inline-flex items-center justify-center w-7 h-8 max-md:w-10 max-md:h-10 text-text-dim hover:text-text-primary cursor-pointer"
                    aria-label={`Remove ${label}`}
                  >
                    &times;
                  </button>
                </span>
                {basesEnabled && (
                  <RepoBaseInput
                    repoPath={path}
                    label={label}
                    value={repoBases[path] ?? ""}
                    onChange={(v) => setRepoBase(path, v)}
                  />
                )}
              </div>
            );
          })}
        </div>
      )}

      {!loading && (saved.length > 0 || recent.length > 0) && (
        <div className="mb-3">
          <ProjectSearchList
            query={query}
            onQueryChange={setQuery}
            filteredSaved={filteredSaved}
            filteredRecent={filteredRecent}
            isSelected={isSelected}
            onSelect={toggle}
            inputRef={searchRef}
          />
        </div>
      )}

      {!loading && saved.length === 0 && recent.length === 0 && (
        <p className={`${FIELD_HINT} mb-3`}>
          {hasAnyProjects ? (
            "No other projects to add — the primary repo is the only one registered or recent."
          ) : (
            <>
              No registered Projects yet. Add one with{" "}
              <code className="text-text-secondary">aoe project add &lt;path&gt;</code> or via the Projects page.
            </>
          )}
        </p>
      )}

      <div className="flex gap-2">
        <input
          type="text"
          value={freeText}
          onChange={(e) => setFreeText(e.target.value)}
          ref={pathRef}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !isImeComposing(e)) {
              e.preventDefault();
              addFreeText();
            }
          }}
          placeholder="/path/to/another/repo"
          className={`${INPUT} flex-1 font-mono`}
        />
        <button type="button" onClick={addFreeText} disabled={!freeText.trim()} className={BUTTON_SECONDARY}>
          Add
        </button>
      </div>
    </div>
  );
}
