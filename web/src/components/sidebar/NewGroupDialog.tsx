import { useMemo, useState } from "react";
import { createGroup, type CreateGroupResult } from "../../lib/api";
import { isImeComposing } from "../../lib/ime";
import type { SessionResponse } from "../../lib/types";
import { SidebarModal } from "./SidebarModal";
import { BUTTON_GHOST, BUTTON_PRIMARY, INPUT } from "../../lib/surfaces";

function refusal(result: Exclude<CreateGroupResult, { ok: true }>): string {
  if (result.code === "group_sessions_required") return "Pick at least one session.";
  if (result.code === "read_only") return "This Daemon is read-only.";
  switch (result.status) {
    case 0:
      return "Couldn't reach the Daemon.";
    case 403:
      return "CityHall creates a group only from sessions in Structured view.";
    case 404:
      return "A picked session is gone. Pick again.";
    case 409:
      return "That group exists already, or a picked session is busy.";
    default:
      return "Couldn't create the group.";
  }
}

/**
 * Creates a Group with the Sessions moved into it, so no Group starts empty. The Daemon moves Sessions of one
 * profile per request, so the list shows one profile at a time.
 */
export function NewGroupDialog({
  sessions,
  selectedId,
  onCancel,
  onCreated,
}: {
  sessions: SessionResponse[];
  /** The selected Session, checked to start with. */
  selectedId: string | null;
  onCancel: () => void;
  onCreated: () => void;
}) {
  const live = useMemo(
    () => sessions.filter((s) => s.trashed_at == null).sort((a, b) => a.title.localeCompare(b.title)),
    [sessions],
  );
  const profiles = useMemo(() => [...new Set(live.map((s) => s.profile))].sort(), [live]);
  const selected = live.find((s) => s.id === selectedId);
  const [profile, setProfile] = useState(selected?.profile ?? profiles[0] ?? "");
  const [name, setName] = useState("");
  const [checked, setChecked] = useState<Set<string>>(() => new Set(selected ? [selected.id] : []));
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const shown = live.filter((s) => s.profile === profile);
  const picked = shown.filter((s) => checked.has(s.id)).map((s) => s.id);

  const toggle = (id: string) => {
    setError(null);
    setChecked((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  };

  const submit = async () => {
    if (busy) return;
    const path = name.trim().replace(/^\/+|\/+$/g, "");
    if (!path) {
      setError("Enter a group name.");
      return;
    }
    if (picked.length === 0) {
      setError("Pick at least one session.");
      return;
    }
    setBusy(true);
    setError(null);
    const result = await createGroup(profile, path, picked);
    setBusy(false);
    if (result.ok) onCreated();
    else setError(refusal(result));
  };

  return (
    <SidebarModal
      testId="new-group-dialog"
      label="New group"
      heading="New group"
      title={profiles.length > 1 ? profile : "with its sessions"}
      locked={busy}
      onCancel={onCancel}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <div className="px-4 py-3 flex flex-col gap-3">
          <input
            type="text"
            autoFocus
            aria-label="Group name"
            disabled={busy}
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              setError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && isImeComposing(e)) e.preventDefault();
            }}
            placeholder="work/api"
            data-testid="new-group-name"
            className={INPUT}
          />
          {profiles.length > 1 && (
            <label className="flex items-center gap-2 text-[13px] text-text-secondary">
              Profile
              <select
                value={profile}
                disabled={busy}
                onChange={(e) => {
                  setProfile(e.target.value);
                  setError(null);
                }}
                data-testid="new-group-profile"
                className={`${INPUT} w-auto flex-1`}
              >
                {profiles.map((p) => (
                  <option key={p} value={p}>
                    {p}
                  </option>
                ))}
              </select>
            </label>
          )}
          <fieldset className="min-w-0">
            <legend className="mb-1 text-[11px] font-mono uppercase tracking-widest text-text-dim">Sessions</legend>
            <div className="max-h-56 overflow-y-auto flex flex-col" data-testid="new-group-sessions">
              {shown.map((s) => (
                <label
                  key={s.id}
                  className="flex items-center gap-2 py-1 text-[13px] text-text-secondary cursor-pointer min-w-0"
                >
                  <input type="checkbox" disabled={busy} checked={checked.has(s.id)} onChange={() => toggle(s.id)} />
                  <span className="font-mono truncate">{s.title}</span>
                  {s.group_path && <span className="shrink-0 text-[11px] text-text-dim">{s.group_path}</span>}
                </label>
              ))}
            </div>
          </fieldset>
          {error && (
            <div role="alert" data-testid="new-group-error" className="text-[11px] text-status-error">
              {error}
            </div>
          )}
        </div>
        <div className="px-4 py-3 border-t border-surface-700/40 flex justify-end gap-2">
          <button type="button" onClick={onCancel} disabled={busy} className={BUTTON_GHOST}>
            Cancel
          </button>
          <button type="submit" disabled={busy} data-testid="new-group-create" className={BUTTON_PRIMARY}>
            {busy ? "Creating…" : "Create"}
          </button>
        </div>
      </form>
    </SidebarModal>
  );
}
