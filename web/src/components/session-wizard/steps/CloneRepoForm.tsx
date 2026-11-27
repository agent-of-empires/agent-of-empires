import { useState, type Ref } from "react";
import { cloneRepo } from "../../../lib/api";
import { isImeComposing } from "../../../lib/ime";
import { BUTTON_PRIMARY, INPUT } from "../../../lib/surfaces";
import { Spinner } from "../../Dialog";
import { FIELD_LABEL } from "../../../lib/surfaces";

export function CloneRepoForm({
  onCloned,
  inputRef,
}: {
  onCloned: (path: string) => void;
  inputRef?: Ref<HTMLInputElement>;
}) {
  const [cloneUrl, setCloneUrl] = useState("");
  const [cloneDestination, setCloneDestination] = useState("");
  const [shallowClone, setShallowClone] = useState(false);
  const [bareClone, setBareClone] = useState(false);
  const [cloning, setCloning] = useState(false);
  const [cloneError, setCloneError] = useState<string | null>(null);
  const [showAdvanced, setShowAdvanced] = useState(false);

  const handleClone = async () => {
    const url = cloneUrl.trim();
    if (!url) return;
    setCloning(true);
    setCloneError(null);
    const result = await cloneRepo(url, {
      destination: cloneDestination.trim() || undefined,
      shallow: shallowClone,
      bare: bareClone,
    });
    setCloning(false);
    if (result.ok && result.path) {
      setCloneUrl("");
      setCloneDestination("");
      onCloned(result.path);
    } else {
      setCloneError(result.error || "Clone failed");
    }
  };

  return (
    <div className="space-y-3">
      <div>
        <label htmlFor="clone-url" className={FIELD_LABEL}>
          Repository URL
        </label>
        <input
          ref={inputRef}
          id="clone-url"
          type="text"
          value={cloneUrl}
          onChange={(e) => {
            setCloneUrl(e.target.value);
            setCloneError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !isImeComposing(e) && cloneUrl.trim() && !cloning) handleClone();
          }}
          placeholder="https://github.com/user/repo.git"
          className={`${INPUT} font-mono`}
          disabled={cloning}
        />
      </div>

      <button
        type="button"
        onClick={() => setShowAdvanced(!showAdvanced)}
        className="min-h-11 md:min-h-0 text-[13px] md:text-xs text-text-dim hover:text-text-secondary cursor-pointer flex items-center gap-1 transition-colors"
      >
        <svg
          className={`w-3 h-3 transition-transform ${showAdvanced ? "rotate-90" : ""}`}
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <polyline points="9 18 15 12 9 6" />
        </svg>
        Advanced
      </button>

      {showAdvanced && (
        <div className="space-y-3 pl-1 border-l-2 border-surface-700/30 ml-1">
          <div>
            <label htmlFor="clone-dest" className={FIELD_LABEL}>
              Destination path (optional)
            </label>
            <input
              id="clone-dest"
              type="text"
              value={cloneDestination}
              onChange={(e) => {
                setCloneDestination(e.target.value);
                setCloneError(null);
              }}
              placeholder="~/my-repo"
              className={`${INPUT} font-mono`}
              disabled={cloning}
            />
          </div>
          <label className="flex items-center gap-2 min-h-11 md:min-h-0 cursor-pointer">
            <input
              type="checkbox"
              checked={shallowClone}
              onChange={(e) => setShallowClone(e.target.checked)}
              className="accent-brand-600"
              disabled={cloning || bareClone}
            />
            <span className={`text-[15px] md:text-sm ${bareClone ? "text-text-dim" : "text-text-secondary"}`}>
              Shallow clone (--depth 1)
            </span>
            <span className="text-[10px] text-text-dim">faster for large repos</span>
          </label>
          <label className="flex items-center gap-2 min-h-11 md:min-h-0 cursor-pointer">
            <input
              type="checkbox"
              checked={bareClone}
              onChange={(e) => {
                setBareClone(e.target.checked);
                if (e.target.checked) setShallowClone(false);
              }}
              className="accent-brand-600"
              disabled={cloning}
            />
            <span className="text-[15px] md:text-sm text-text-secondary">Clone as bare repository</span>
            <span className="text-[10px] text-text-dim">recommended for worktrees</span>
          </label>
        </div>
      )}

      {cloneError && (
        <div className="rounded-lg bg-status-error/10 p-3">
          <p className="text-[15px] md:text-sm text-status-error">{cloneError}</p>
        </div>
      )}

      <button
        type="button"
        onClick={handleClone}
        disabled={!cloneUrl.trim() || cloning}
        className={`${BUTTON_PRIMARY} w-full`}
      >
        {cloning ? (
          <>
            <Spinner />
            Cloning...
          </>
        ) : (
          "Clone repository"
        )}
      </button>

      <div className="flex items-start gap-1.5 text-[11px] text-text-dim">
        <span>The repository will be cloned into your home directory.</span>
        <span className="relative group/info inline-flex shrink-0 mt-px">
          <svg
            className="w-3.5 h-3.5 text-text-dim cursor-help"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <circle cx="12" cy="12" r="10" />
            <path d="M12 16v-4" />
            <path d="M12 8h.01" />
          </svg>
          <span className="pointer-events-none absolute right-0 bottom-full mb-1.5 w-56 px-2.5 py-2 rounded bg-surface-950 border border-surface-700 text-[11px] leading-relaxed text-text-secondary opacity-0 scale-95 transition-all duration-100 group-hover/info:opacity-100 group-hover/info:scale-100 z-50">
            Uses the git credentials from the environment where the server is running (SSH keys, credential helpers,
            GH_TOKEN, etc). Private repos work if your git is already authenticated.
          </span>
        </span>
      </div>
    </div>
  );
}
