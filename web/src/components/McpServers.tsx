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
import { BACKDROP, BUTTON_GHOST, BUTTON_PRIMARY, BUTTON_SECONDARY, CARD, DIALOG } from "../lib/surfaces";
import { ProvenanceBadge } from "./ProvenanceBadge";
import { BUTTON_DANGER_GHOST_SM, BUTTON_PRIMARY_SM, BUTTON_SECONDARY_SM, ROWS } from "../lib/surfaces";
import { Notice, PanelMessage, SectionHeading } from "./panelParts";
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
    <div className="py-3">
      <div className="flex items-center gap-2">
        <span className="text-[15px] md:text-sm font-medium text-text-primary">{s.name}</span>
        <span className="font-mono text-[11px] text-text-muted">({s.transport})</span>
        <ProvenanceBadge label={s.provenance} />
        {onEdit && onDelete && (
          <span className="ml-auto flex gap-1">
            <button
              type="button"
              disabled={busy}
              onClick={onEdit}
              aria-label={`edit ${s.name}`}
              className={BUTTON_SECONDARY_SM}
            >
              Edit
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={onDelete}
              aria-label={`delete ${s.name}`}
              className={BUTTON_DANGER_GHOST_SM}
            >
              Delete
            </button>
          </span>
        )}
      </div>
      <p className="mt-0.5 break-all font-mono text-[12px] text-text-secondary">{detail(s)}</p>
      {s.shadowed && s.shadowed.length > 0 && (
        <p className="mt-0.5 text-[12px] text-text-dim">shadows: {s.shadowed.join(", ")}</p>
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
      className={`fixed inset-0 ${BACKDROP} flex items-center justify-center z-50 animate-fade-in`}
    >
      <div className={`${DIALOG} p-5 w-[min(36rem,90vw)]`}>
        <h3 className="text-[15px] font-semibold text-text-primary mb-2">
          Conflict: <span className="font-mono">{conflict.name}</span>
        </h3>
        <p className="text-[13px] leading-normal text-text-secondary mb-3">
          This server's definition in the agent's native config changed since AoE last saw it. Pick which side wins. AoE
          never writes back to the native config; keeping AoE's version stores it in the global mcp.json.
        </p>
        <div className="space-y-2 mb-4">
          <div>
            <span className="text-[12px] text-text-dim">AoE (last seen):</span>
            <p className="break-all font-mono text-[12px] text-text-primary">{conflict.previous}</p>
          </div>
          <div>
            <span className="text-[12px] text-text-dim">native (now):</span>
            <p className="break-all font-mono text-[12px] text-text-primary">{conflict.current}</p>
          </div>
        </div>
        <div className="flex justify-end gap-2">
          <button type="button" disabled={busy} onClick={onClose} className={BUTTON_GHOST}>
            Cancel
          </button>
          <button type="button" disabled={busy} onClick={() => onResolve("native")} className={BUTTON_SECONDARY}>
            Use native
          </button>
          <button type="button" disabled={busy} onClick={() => onResolve("aoe")} className={BUTTON_PRIMARY}>
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

  if (error || data === null) {
    return (
      <div>
        <SectionHeading title="MCP Servers" />
        <PanelMessage tone={error ? "error" : "dim"} className="px-1">
          {error ? "Could not load MCP servers" : "Loading..."}
        </PanelMessage>
      </div>
    );
  }

  return (
    <div data-testid="mcp-panel" className="space-y-6">
      <div>
        <SectionHeading
          title="MCP Servers"
          description={
            <>
              Effective set forwarded to <span className="font-mono">{agent}</span>, with provenance. Values are
              redacted; only env and header names are shown.
            </>
          }
        />

        {notice && (
          <PanelMessage tone="warning" role="status" className="px-1 mb-2">
            {notice}
          </PanelMessage>
        )}

        {data.driftPaused && (
          <PanelMessage tone="warning" className="px-1 mb-2">
            Drift detection is paused: the native config for {agent} has a malformed entry.
          </PanelMessage>
        )}

        {data.conflicts.length > 0 && (
          <Notice tone="error" title="Conflicts" testId="mcp-conflicts" className="mb-3">
            <div className="divide-y divide-status-error/20">
              {data.conflicts.map((c) => (
                <div key={c.name} className="flex items-center justify-between gap-3 py-1.5">
                  <span className="font-mono text-text-primary">{c.name}</span>
                  {!readOnly && (
                    <button
                      type="button"
                      onClick={() => setActive(c)}
                      aria-label={`resolve ${c.name}`}
                      className={BUTTON_SECONDARY_SM}
                    >
                      Resolve
                    </button>
                  )}
                </div>
              ))}
            </div>
          </Notice>
        )}

        {data.effective.length === 0 ? (
          <div className={`${CARD} py-3`}>
            <PanelMessage>No servers forwarded.</PanelMessage>
          </div>
        ) : (
          <div className={`${CARD} ${ROWS}`}>
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
            className={`mt-3 ${BUTTON_PRIMARY}`}
          >
            Add server
          </button>
        )}
      </div>

      {data.keptOnRemoval.length > 0 && (
        <div data-testid="mcp-kept">
          <SectionHeading
            as="h4"
            title="Kept after removal from the native config"
            description="These are no longer in the native config and are not forwarded. Keep promotes them to the global mcp.json; drop discards them."
          />
          <div className={`${CARD} ${ROWS}`}>
            {data.keptOnRemoval.map((s) => (
              <div key={s.name} className="flex items-center justify-between gap-3 py-3">
                <div className="min-w-0">
                  <span className="text-[15px] md:text-sm text-text-primary">{s.name}</span>
                  <p className="mt-0.5 break-all font-mono text-[12px] text-text-secondary">{detail(s)}</p>
                </div>
                {!readOnly && (
                  <div className="flex shrink-0 gap-1">
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => onKeep(s.name)}
                      aria-label={`keep ${s.name}`}
                      className={BUTTON_PRIMARY_SM}
                    >
                      Keep
                    </button>
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => onDrop(s.name)}
                      aria-label={`drop ${s.name}`}
                      className={BUTTON_SECONDARY_SM}
                    >
                      Drop
                    </button>
                  </div>
                )}
              </div>
            ))}
          </div>
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
