import { useState } from "react";
import type { McpSecretEntry, McpServerInput, McpServerView } from "../lib/api";

type Transport = McpServerInput["transport"];

interface SecretRow {
  name: string;
  value: string;
  /** Row came from the stored server: an empty value keeps the stored secret. */
  stored: boolean;
}

const inputClass =
  "w-full bg-surface-900 border border-surface-700 rounded px-2 py-1 font-mono text-[12px] text-text-primary";

function toEntries(rows: SecretRow[]): McpSecretEntry[] {
  return rows
    .filter((r) => r.name.trim() !== "")
    .map((r) => (r.stored && r.value === "" ? { name: r.name } : { name: r.name, value: r.value }));
}

function SecretRows({
  label,
  rows,
  onChange,
}: {
  label: string;
  rows: SecretRow[];
  onChange: (rows: SecretRow[]) => void;
}) {
  const update = (i: number, patch: Partial<SecretRow>) =>
    onChange(rows.map((r, idx) => (idx === i ? { ...r, ...patch } : r)));
  return (
    <div className="space-y-1">
      <span className="font-mono text-[11px] text-text-muted">{label}</span>
      {rows.map((r, i) => (
        <div key={i} className="flex gap-2">
          <input
            aria-label={`${label} name ${i + 1}`}
            className={inputClass}
            value={r.name}
            readOnly={r.stored}
            onChange={(e) => update(i, { name: e.target.value })}
          />
          <input
            aria-label={`${label} value ${i + 1}`}
            type="password"
            autoComplete="off"
            className={inputClass}
            value={r.value}
            placeholder={r.stored ? "unchanged" : "value"}
            onChange={(e) => update(i, { value: e.target.value })}
          />
          <button
            type="button"
            aria-label={`remove ${label} ${i + 1}`}
            onClick={() => onChange(rows.filter((_, idx) => idx !== i))}
            className="px-2 text-[12px] rounded border border-surface-700 text-text-secondary hover:bg-surface-700"
          >
            Remove
          </button>
        </div>
      ))}
      <button
        type="button"
        onClick={() => onChange([...rows, { name: "", value: "", stored: false }])}
        className="px-2 py-1 text-[12px] rounded border border-surface-700 text-text-secondary hover:bg-surface-700"
      >
        Add {label}
      </button>
    </div>
  );
}

/** One editor per argument, so empty and multiline arguments survive an unchanged save. */
function ArgRows({ rows, onChange }: { rows: string[]; onChange: (rows: string[]) => void }) {
  return (
    <div className="space-y-1">
      <span className="font-mono text-[11px] text-text-muted">Arguments</span>
      {rows.map((a, i) => (
        <div key={i} className="flex gap-2">
          <textarea
            aria-label={`argument ${i + 1}`}
            className={inputClass}
            rows={1}
            value={a}
            onChange={(e) => onChange(rows.map((r, idx) => (idx === i ? e.target.value : r)))}
          />
          <button
            type="button"
            aria-label={`remove argument ${i + 1}`}
            onClick={() => onChange(rows.filter((_, idx) => idx !== i))}
            className="px-2 text-[12px] rounded border border-surface-700 text-text-secondary hover:bg-surface-700"
          >
            Remove
          </button>
        </div>
      ))}
      <button
        type="button"
        onClick={() => onChange([...rows, ""])}
        className="px-2 py-1 text-[12px] rounded border border-surface-700 text-text-secondary hover:bg-surface-700"
      >
        Add argument
      </button>
    </div>
  );
}

export function McpServerForm({
  initial,
  busy,
  error,
  onSubmit,
  onClose,
}: {
  initial?: McpServerView;
  busy: boolean;
  error: string | null;
  onSubmit: (name: string, input: McpServerInput) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [transport, setTransport] = useState<Transport>((initial?.transport as Transport | undefined) ?? "stdio");
  const [command, setCommand] = useState(initial?.command ?? "");
  const [args, setArgs] = useState<string[]>(initial?.args ?? []);
  const [url, setUrl] = useState(initial?.url ?? "");
  const storedRows = (names?: string[]): SecretRow[] =>
    (names ?? []).map((n) => ({ name: n, value: "", stored: true }));
  const [env, setEnv] = useState<SecretRow[]>(storedRows(initial?.envNames));
  const [headers, setHeaders] = useState<SecretRow[]>(storedRows(initial?.headerNames));

  const submit = () => {
    const input: McpServerInput =
      transport === "stdio"
        ? { transport, command, args, env: toEntries(env) }
        : { transport, url, headers: toEntries(headers) };
    onSubmit(name, input);
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={initial ? `Edit MCP server ${initial.name}` : "Add MCP server"}
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
    >
      <div className="bg-surface-800 border border-surface-700/50 rounded-lg p-5 w-[min(36rem,90vw)] max-h-[90vh] overflow-y-auto">
        <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-3">
          {initial ? `Edit ${initial.name}` : "Add server"}
        </h3>
        <div className="space-y-3">
          <label className="block">
            <span className="font-mono text-[11px] text-text-muted">Name</span>
            <input
              aria-label="Name"
              className={inputClass}
              value={name}
              readOnly={!!initial}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="block">
            <span className="font-mono text-[11px] text-text-muted">Transport</span>
            <select
              aria-label="Transport"
              className={inputClass}
              value={transport}
              onChange={(e) => setTransport(e.target.value as Transport)}
            >
              <option value="stdio">stdio</option>
              <option value="http">http</option>
              <option value="sse">sse</option>
            </select>
          </label>
          {transport === "stdio" ? (
            <>
              <p className="font-body text-[12px] text-status-waiting">
                The command runs on this machine with your permissions whenever an agent starts.
              </p>
              <label className="block">
                <span className="font-mono text-[11px] text-text-muted">Command</span>
                <input
                  aria-label="Command"
                  className={inputClass}
                  value={command}
                  onChange={(e) => setCommand(e.target.value)}
                />
              </label>
              <ArgRows rows={args} onChange={setArgs} />
              <SecretRows label="env" rows={env} onChange={setEnv} />
            </>
          ) : (
            <>
              <label className="block">
                <span className="font-mono text-[11px] text-text-muted">URL</span>
                <input aria-label="URL" className={inputClass} value={url} onChange={(e) => setUrl(e.target.value)} />
              </label>
              <SecretRows label="header" rows={headers} onChange={setHeaders} />
            </>
          )}
        </div>
        {error && (
          <p className="font-body text-[12px] text-status-error mt-3" role="alert">
            {error}
          </p>
        )}
        <div className="flex justify-end gap-2 mt-4">
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
            onClick={submit}
            className="px-3 py-1.5 text-[13px] rounded bg-brand-600 text-white hover:bg-brand-500 disabled:opacity-50"
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
