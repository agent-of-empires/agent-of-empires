import { useState } from "react";
import type { AttachProjectResult } from "../../lib/api";
import { SidebarModal } from "./SidebarModal";
import { MODAL_CANCEL, MODAL_INPUT, MODAL_PRIMARY } from "./styles";

function workerSummary(res: AttachProjectResult): string {
  switch (res.worker) {
    case "restarted":
      return "The agent is restarting; your conversation is preserved.";
    case "restart_failed":
      return res.message
        ? `The repo is attached, but the session did not restart: ${res.message}`
        : "The repo is attached, but the session did not restart.";
    default:
      return "The repo is attached; nothing had to be restarted.";
  }
}

/** Attaches another repo to an existing session. Reports the worker outcome instead of
 *  closing on success, since a 200 can still mean the agent did not restart. */
export function AddProjectModal({
  title,
  projects,
  onCancel,
  onSubmit,
  onDone,
}: {
  title: string;
  projects: { name: string; path: string }[];
  onCancel: () => void;
  onSubmit: (project: string, attachExistingBranch: boolean) => Promise<AttachProjectResult>;
  onDone: () => void;
}) {
  const [project, setProject] = useState("");
  const [attachExistingBranch, setAttachExistingBranch] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<AttachProjectResult | null>(null);

  const submit = async () => {
    if (busy) return;
    const trimmed = project.trim();
    if (!trimmed) {
      setError("Pick a project or enter a repo path.");
      return;
    }
    setBusy(true);
    setError(null);
    const res = await onSubmit(trimmed, attachExistingBranch);
    setBusy(false);
    if (!res.ok) {
      setError(res.message ?? "Failed to attach the project.");
      return;
    }
    setResult(res);
  };

  // Dismissal is locked mid-request: the attach lands either way, and closing would hide its outcome.
  return (
    <SidebarModal
      testId="add-project-modal"
      label="Add project"
      heading="Add project"
      title={title}
      locked={busy}
      onCancel={onCancel}
    >
      {result ? (
        <AttachResult result={result} />
      ) : (
        <div className="px-4 py-3 flex flex-col gap-3">
          <input
            type="text"
            autoFocus
            aria-label="Project to attach"
            list="add-project-options"
            disabled={busy}
            value={project}
            onChange={(e) => {
              setProject(e.target.value);
              setError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void submit();
              }
            }}
            placeholder="project name or /path/to/repo"
            data-testid="add-project-modal-input"
            className={MODAL_INPUT}
          />
          <datalist id="add-project-options">
            {projects.map((p) => (
              <option key={p.path} value={p.name}>
                {p.path}
              </option>
            ))}
          </datalist>
          <label className="flex items-center gap-2 text-sm text-text-secondary cursor-pointer">
            <input
              type="checkbox"
              disabled={busy}
              checked={attachExistingBranch}
              onChange={(e) => setAttachExistingBranch(e.target.checked)}
              data-testid="add-project-modal-attach-existing-branch"
            />
            Reuse a branch that already exists there
          </label>
          <div data-testid="add-project-modal-restart-warning" className="text-[11px] text-status-warning">
            Attaching turns this session into a multi-repo workspace. Unless it already is one, its working directory
            moves, so the session and its agent worker are stopped for the move and started again. Your conversation is
            kept.
          </div>
          {error && (
            <div data-testid="add-project-modal-error" className="text-[11px] text-status-error">
              {error}
            </div>
          )}
        </div>
      )}

      <div className="px-4 py-3 border-t border-surface-700/40 flex justify-end gap-2">
        {result ? (
          <button onClick={onDone} data-testid="add-project-modal-done" className={MODAL_PRIMARY}>
            Done
          </button>
        ) : (
          <>
            <button onClick={onCancel} className={MODAL_CANCEL}>
              Cancel
            </button>
            <button
              onClick={() => void submit()}
              disabled={busy}
              data-testid="add-project-modal-submit"
              className={`${MODAL_PRIMARY} disabled:opacity-50`}
            >
              {busy ? "Attaching…" : "Attach"}
            </button>
          </>
        )}
      </div>
    </SidebarModal>
  );
}

function AttachResult({ result }: { result: AttachProjectResult }) {
  return (
    <div className="px-4 py-3 flex flex-col gap-2">
      <div data-testid="add-project-modal-result" className="text-[13px] text-text-primary">
        Attached <span className="font-mono">{result.name}</span>
        {result.branch && (
          <>
            {" on "}
            <span className="font-mono">{result.branch}</span>
            {result.branchCreated === false && <span className="text-text-dim"> (existing branch, left in place)</span>}
          </>
        )}
      </div>
      {result.movedTo && (
        <div data-testid="add-project-modal-moved-to" className="text-[11px] text-text-dim">
          This session is now a multi-repo workspace; its working directory moved to{" "}
          <span className="font-mono break-all">{result.movedTo}</span>
        </div>
      )}
      <div className="text-[11px] text-text-dim">{workerSummary(result)}</div>
      {result.warnings?.map((w) => (
        <div key={w} className="text-[11px] text-status-warning">
          {w}
        </div>
      ))}
    </div>
  );
}
