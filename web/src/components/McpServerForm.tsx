import { useState } from "react";
import type { McpSecretEntry, McpServerInput, McpServerView } from "../lib/api";
import { hasFinePointer } from "../lib/platform";
import { BUTTON_GHOST, BUTTON_SECONDARY, INPUT } from "../lib/surfaces";
import { CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { FIELD_LABEL } from "../lib/surfaces";

type Transport = McpServerInput["transport"];

interface SecretRow {
  name: string;
  value: string;
  /** Row came from the stored server: an empty value keeps the stored secret. */
  stored: boolean;
}

const inputClass = `${INPUT} font-mono`;
const LABEL = `${FIELD_LABEL} !mb-1`;
const ROW_BUTTON = `${BUTTON_GHOST} shrink-0 !px-3`;
const ADD_BUTTON = `${BUTTON_SECONDARY} self-start`;

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
    <div className="flex flex-col gap-1.5">
      <span className={LABEL}>{label}</span>
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
            className={ROW_BUTTON}
          >
            Remove
          </button>
        </div>
      ))}
      <button
        type="button"
        onClick={() => onChange([...rows, { name: "", value: "", stored: false }])}
        className={ADD_BUTTON}
      >
        Add {label}
      </button>
    </div>
  );
}

/** One editor per argument, so empty and multiline arguments survive an unchanged save. */
function ArgRows({ rows, onChange }: { rows: string[]; onChange: (rows: string[]) => void }) {
  return (
    <div className="flex flex-col gap-1.5">
      <span className={LABEL}>Arguments</span>
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
            className={ROW_BUTTON}
          >
            Remove
          </button>
        </div>
      ))}
      <button type="button" onClick={() => onChange([...rows, ""])} className={ADD_BUTTON}>
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
    <Dialog
      id="mcp-server-form"
      ariaLabel={initial ? `Edit MCP server ${initial.name}` : "Add MCP server"}
      title={initial ? `Edit ${initial.name}` : "Add server"}
      describedBy={false}
      panelClassName="md:w-[36rem] md:max-h-[90vh]"
      footer={
        <>
          <CancelButton onClick={onClose} disabled={busy} />
          <ConfirmButton onClick={submit} disabled={busy}>
            Save
          </ConfirmButton>
        </>
      }
    >
      <div className="space-y-3">
        <label className="block">
          <span className={LABEL}>Name</span>
          <input
            aria-label="Name"
            autoFocus={!initial && hasFinePointer()}
            className={inputClass}
            value={name}
            readOnly={!!initial}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        <label className="block">
          <span className={LABEL}>Transport</span>
          <select
            aria-label="Transport"
            autoFocus={!!initial && hasFinePointer()}
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
              <span className={LABEL}>Command</span>
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
              <span className={LABEL}>URL</span>
              <input aria-label="URL" className={inputClass} value={url} onChange={(e) => setUrl(e.target.value)} />
            </label>
            <SecretRows label="header" rows={headers} onChange={setHeaders} />
          </>
        )}
      </div>
      {error && (
        <p className="text-[13px] text-status-error mt-3" role="alert">
          {error}
        </p>
      )}
    </Dialog>
  );
}
