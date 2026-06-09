import { useEffect, useState } from "react";
import { fetchAbout, type ServerAbout } from "../lib/api";

function Row({ label, value }: { label: string; value: string | React.ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-4 py-2 border-b border-surface-700/40 last:border-0">
      <span className="font-mono text-[11px] uppercase tracking-wider text-text-muted">{label}</span>
      <span className="text-[13px] text-text-primary text-right">{value}</span>
    </div>
  );
}

function Badge({ tone, children }: { tone: "ok" | "warn" | "muted"; children: React.ReactNode }) {
  const cls =
    tone === "ok"
      ? "bg-status-running/15 text-status-running"
      : tone === "warn"
        ? "bg-status-waiting/15 text-status-waiting"
        : "bg-surface-800 text-text-muted";
  return (
    <span className={`font-mono text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded ${cls}`}>{children}</span>
  );
}

export function SecuritySettings() {
  const [about, setAbout] = useState<ServerAbout | null>(null);
  const [loadError, setLoadError] = useState(false);

  useEffect(() => {
    fetchAbout().then((a) => {
      if (a) setAbout(a);
      else setLoadError(true);
    });
  }, []);

  return (
    <div>
      <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted mb-4">Security</h3>

      {loadError && <p className="text-[13px] text-status-error mb-3">Could not load server status.</p>}

      <div className="rounded-lg border border-surface-700/50 bg-surface-900 px-4 py-2">
        <Row
          label="Auth mode"
          value={
            about === null ? (
              <span className="text-text-muted">…</span>
            ) : about.auth_mode === "token" ? (
              <Badge tone="ok">--auth=token</Badge>
            ) : about.auth_mode === "passphrase" ? (
              <Badge tone="ok">--auth=passphrase</Badge>
            ) : (
              <Badge tone="warn">--auth=none</Badge>
            )
          }
        />
        <Row
          label="Passphrase"
          value={
            about === null ? (
              <span className="text-text-muted">…</span>
            ) : about.passphrase_enabled ? (
              <Badge tone="ok">required</Badge>
            ) : (
              <Badge tone="muted">not set</Badge>
            )
          }
        />
        <Row
          label="Read-only"
          value={
            about === null ? (
              <span className="text-text-muted">…</span>
            ) : about.read_only ? (
              <Badge tone="ok">on (terminal input blocked)</Badge>
            ) : (
              <Badge tone="muted">off</Badge>
            )
          }
        />
        <Row
          label="Tunnel"
          value={
            about === null ? (
              <span className="text-text-muted">…</span>
            ) : about.behind_tunnel ? (
              <Badge tone="ok">cloudflared</Badge>
            ) : (
              <Badge tone="muted">local only</Badge>
            )
          }
        />
        <Row
          label="Version"
          value={
            about?.version ? (
              <span className="font-mono text-text-primary">v{about.version}</span>
            ) : (
              <span className="text-text-muted">unknown</span>
            )
          }
        />
      </div>

      <p className="mt-3 text-[11px] text-text-dim">
        Security settings are configured at launch via <code className="font-mono text-text-muted">aoe serve</code>{" "}
        flags. See the{" "}
        <a
          href="https://agent-of-empires.com/guides/web-dashboard/"
          target="_blank"
          rel="noopener noreferrer"
          className="text-brand-500 hover:text-brand-400 underline decoration-brand-500/30"
        >
          web dashboard guide
        </a>{" "}
        for details.
      </p>
    </div>
  );
}
