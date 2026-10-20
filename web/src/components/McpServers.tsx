import { useCallback, useEffect, useState } from "react";
import {
  fetchMcpServers,
  resolveMcpConflict,
  keepMcpServer,
  dropMcpServer,
  createMcpServer,
  updateMcpServer,
  deleteMcpServer,
  type McpServerInput,
  type McpServersResponse,
  type McpServerView,
  type McpConflictView,
} from "../lib/api";
import { ProvenanceBadge } from "./ProvenanceBadge";
import { McpServerForm } from "./McpServerForm";

/** One-line redacted connection detail: command/args or url, plus secret NAMES. */
function detail(s: McpServerView): string {
  let base = s.command ? `${s.command}${s.args && s.args.length ? " " + s.args.join(" ") : ""}` : (s.url ?? "");
  const tags: string[] = [];
  if (s.envNames && s.envNames.length) tags.push(`env: ${s.envNames.join(", ")}`);
  if (s.headerNames && s.headerNames.length) tags.push(`headers: ${s.headerNames.join(", ")}`);
  if (tags.length) base += `  [${tags.join("; ")}]`;
  return base;
}

function ServerRow({
  s,
  busy,
  onEdit,
  onDelete,
}: {
  s: McpServerView;
  busy: boolean;
  onEdit?: () => void;
  onDelete?: () => void;
}) {
  return (
    <div className="py-2 border-b border-surface-700">
      <div className="flex items-center gap-2">
        <span className="font-body text-[13px] font-medium text-text-primary">{s.name}</span>
        <span className="font-mono text-[11px] text-text-muted">({s.transport})</span>
        <ProvenanceBadge label={s.provenance} />
        {onEdit && onDelete && (
          <span className="ml-auto flex gap-2">
            <button
              type="button"
              disabled={busy}
              onClick={onEdit}
              aria-label={`edit ${s.name}`}
              className="px-2.5 py-1 text-[12px] rounded border border-surface-700 text-text-secondary hover:bg-surface-700 disabled:opacity-50"
            >
              Edit
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={onDelete}
              aria-label={`delete ${s.name}`}
              className="px-2.5 py-1 text-[12px] rounded border border-status-error/50 text-status-error hover:bg-status-error/10 disabled:opacity-50"
            >
              Delete
            </button>
          </span>
        )}
      </div>
      <p className="font-mono text-[11px] text-text-secondary ml-1">{detail(s)}</p>
      {s.shadowed && s.shadowed.length > 0 && (
        <p className="font-body text-[11px] text-text-muted ml-1">shadows: {s.shadowed.join(", ")}</p>
      )}
    </div>
  );
}

function ConflictModal({
  conflict,
  busy,
  onResolve,
  onClose,
}: {
  conflict: McpConflictView;
  busy: boolean;
  onResolve: (winner: "aoe" | "native") => void;
  onClose: () => void;
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={`Resolve MCP conflict for ${conflict.name}`}
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
    >
      <div className="bg-surface-800 border border-surface-700/50 rounded-lg p-5 w-[min(36rem,90vw)]">
        <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-2">Conflict: {conflict.name}</h3>
        <p className="font-body text-[13px] text-text-secondary mb-3">
          This server's definition in the agent's native config changed since AoE last saw it. Pick which side wins. AoE
          never writes back to the native config; keeping AoE's version stores it in the global mcp.json.
        </p>
        <div className="space-y-2 mb-4">
          <div>
            <span className="font-mono text-[11px] text-text-muted">AoE (last seen):</span>
            <p className="font-mono text-[12px] text-text-primary">{conflict.previous}</p>
          </div>
          <div>
            <span className="font-mono text-[11px] text-text-muted">native (now):</span>
            <p className="font-mono text-[12px] text-text-primary">{conflict.current}</p>
          </div>
        </div>
        <div className="flex justify-end gap-2">
          <button
            type="button"
            disabled={busy}
            onClick={onClose}
            className="px-3 py-1.5 text-[13px] rounded border border-surface-700 text-text-secondary hover:bg-surface-700 disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => onResolve("native")}
            className="px-3 py-1.5 text-[13px] rounded border border-surface-700 text-text-primary hover:bg-surface-700 disabled:opacity-50"
          >
            Use native
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => onResolve("aoe")}
            className="px-3 py-1.5 text-[13px] rounded bg-brand-600 text-white hover:bg-brand-500 disabled:opacity-50"
          >
            Keep AoE version
          </button>
        </div>
      </div>
    </div>
  );
}

export function McpServers({ readOnly = false }: { readOnly?: boolean } = {}) {
  const [data, setData] = useState<McpServersResponse | null>(null);
  const [error, setError] = useState(false);
  const [active, setActive] = useState<McpConflictView | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [form, setForm] = useState<{ server?: McpServerView } | null>(null);
  const [formError, setFormError] = useState<string | null>(null);

  const load = useCallback(async () => {
    const result = await fetchMcpServers();
    if (result === null) {
      setError(true);
    } else {
      setError(false);
      setData(result);
    }
  }, []);

  useEffect(() => {
    const first = setTimeout(load, 0);
    return () => clearTimeout(first);
  }, [load]);

  const agent = data?.agent ?? "";

  const onResolve = async (winner: "aoe" | "native") => {
    if (!active) return;
    setBusy(true);
    const result = await resolveMcpConflict(active.name, agent, winner, active.fingerprint);
    setBusy(false);
    setActive(null);
    if (result === "stale") {
      setNotice(`"${active.name}" was already resolved by another surface; refreshed.`);
    } else if (result === "error") {
      setNotice(`Could not resolve "${active.name}".`);
    } else {
      setNotice(null);
    }
    await load();
  };

  const onKeep = async (name: string) => {
    setBusy(true);
    const ok = await keepMcpServer(name, agent);
    setBusy(false);
    if (!ok) {
      setNotice(`Could not keep "${name}".`);
      return;
    }
    setNotice(null);
    await load();
  };

  const onDrop = async (name: string) => {
    setBusy(true);
    const ok = await dropMcpServer(name, agent);
    setBusy(false);
    if (!ok) {
      setNotice(`Could not drop "${name}".`);
      return;
    }
    setNotice(null);
    await load();
  };

  const onSave = async (name: string, input: McpServerInput) => {
    setBusy(true);
    const result = form?.server ? await updateMcpServer(name, input) : await createMcpServer(name, input);
    setBusy(false);
    if (!result.ok) {
      setFormError(result.error ?? "Could not save the server.");
      return;
    }
    setForm(null);
    setFormError(null);
    setNotice(null);
    await load();
  };

  const onDelete = async (name: string) => {
    if (!window.confirm(`Delete MCP server "${name}"?`)) return;
    setBusy(true);
    const result = await deleteMcpServer(name);
    setBusy(false);
    setNotice(result.ok ? null : (result.error ?? `Could not delete "${name}".`));
    await load();
  };

  if (error) {
    return (
      <div>
        <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-4">MCP Servers</h3>
        <p className="font-body text-[13px] text-status-error">Could not load MCP servers</p>
      </div>
    );
  }

  if (data === null) {
    return (
      <div>
        <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-4">MCP Servers</h3>
        <p className="font-mono text-[11px] text-text-muted">Loading...</p>
      </div>
    );
  }

  return (
    <div data-testid="mcp-panel">
      <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-1">MCP Servers</h3>
      <p className="font-body text-[12px] text-text-muted mb-4">
        Effective set forwarded to <span className="font-mono">{agent}</span>, with provenance. Values are redacted;
        only env and header names are shown.
      </p>

      {notice && (
        <p className="font-body text-[12px] text-status-waiting mb-3" role="status">
          {notice}
        </p>
      )}

      {data.conflicts.length > 0 && (
        <div className="mb-5" data-testid="mcp-conflicts">
          <h4 className="font-mono text-[11px] uppercase tracking-wider text-status-error mb-2">Conflicts</h4>
          {data.conflicts.map((c) => (
            <div key={c.name} className="flex items-center justify-between py-2 border-b border-surface-700">
              <span className="font-body text-[13px] text-text-primary">{c.name}</span>
              {!readOnly && (
                <button
                  type="button"
                  onClick={() => setActive(c)}
                  aria-label={`resolve ${c.name}`}
                  className="px-2.5 py-1 text-[12px] rounded border border-status-error/50 text-status-error hover:bg-status-error/10"
                >
                  Resolve
                </button>
              )}
            </div>
          ))}
        </div>
      )}

      {data.driftPaused && (
        <p className="font-body text-[12px] text-status-waiting mb-4">
          Drift detection is paused: the native config for {agent} has a malformed entry.
        </p>
      )}

      {data.effective.length === 0 ? (
        <p className="font-body text-[13px] text-text-muted">No servers forwarded.</p>
      ) : (
        <div className="mb-5">
          {data.effective.map((s) => {
            const owned = !readOnly && s.provenance === "global";
            return (
              <ServerRow
                key={s.name}
                s={s}
                busy={busy}
                onEdit={
                  owned
                    ? () => {
                        setFormError(null);
                        setForm({ server: s });
                      }
                    : undefined
                }
                onDelete={owned ? () => onDelete(s.name) : undefined}
              />
            );
          })}
        </div>
      )}

      {!readOnly && (
        <button
          type="button"
          onClick={() => {
            setFormError(null);
            setForm({});
          }}
          className="mb-5 px-3 py-1.5 text-[13px] rounded bg-brand-600 text-white hover:bg-brand-500"
        >
          Add server
        </button>
      )}

      {data.keptOnRemoval.length > 0 && (
        <div data-testid="mcp-kept">
          <h4 className="font-mono text-[11px] uppercase tracking-wider text-status-waiting mb-2">
            Kept after removal from the native config
          </h4>
          <p className="font-body text-[12px] text-text-muted mb-2">
            These are no longer in the native config and are not forwarded. Keep promotes them to the global mcp.json;
            drop discards them.
          </p>
          {data.keptOnRemoval.map((s) => (
            <div key={s.name} className="flex items-center justify-between py-2 border-b border-surface-700">
              <div>
                <span className="font-body text-[13px] text-text-primary">{s.name}</span>
                <p className="font-mono text-[11px] text-text-secondary">{detail(s)}</p>
              </div>
              {!readOnly && (
                <div className="flex gap-2">
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => onKeep(s.name)}
                    aria-label={`keep ${s.name}`}
                    className="px-2.5 py-1 text-[12px] rounded bg-brand-600 text-white hover:bg-brand-500 disabled:opacity-50"
                  >
                    Keep
                  </button>
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => onDrop(s.name)}
                    aria-label={`drop ${s.name}`}
                    className="px-2.5 py-1 text-[12px] rounded border border-surface-700 text-text-secondary hover:bg-surface-700 disabled:opacity-50"
                  >
                    Drop
                  </button>
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      {form && (
        <McpServerForm
          initial={form.server}
          busy={busy}
          error={formError}
          onSubmit={onSave}
          onClose={() => setForm(null)}
        />
      )}

      {active && <ConflictModal conflict={active} busy={busy} onResolve={onResolve} onClose={() => setActive(null)} />}
    </div>
  );
}
