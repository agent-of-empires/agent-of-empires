import { useState } from "react";

import { InstallAgentError, installAcpAgent } from "../../lib/api";

/** Copy for each `install-agent` refusal; any other failure shows the server's message. */
const REFUSAL_COPY: Record<string, string> = {
  install_disabled:
    'Installing from the dashboard is off. Turn on "Allow agent install from web" in the aoe TUI settings (Global, Acp tab, Advanced) to allow it.',
  npm_missing:
    "npm isn't on the Daemon's PATH. Restart aoe serve from a shell where `which npm` works, or run the install command yourself.",
  not_npm_installable:
    "The Daemon can't install this ACP adapter with npm. Run its install command where the Daemon runs instead.",
  container_not_running: "The Session's sandbox container isn't running. Open the Session to start it, then try again.",
};

function installFailureMessage(e: unknown): string {
  if (e instanceof InstallAgentError && e.code && REFUSAL_COPY[e.code]) return REFUSAL_COPY[e.code]!;
  return e instanceof Error ? e.message : String(e);
}

/** Runs `install-agent` for the Session's adapter, then `respawn` once it succeeds. */
export function useAdapterInstall(sessionId: string, respawn: () => Promise<boolean>) {
  const [state, setState] = useState<"idle" | "installing" | "failed">("idle");
  const [error, setError] = useState<string | null>(null);
  const [output, setOutput] = useState<string | null>(null);
  const [recovered, setRecovered] = useState(0);

  const install = async () => {
    setState("installing");
    setError(null);
    setOutput(null);
    try {
      const res = await installAcpAgent(sessionId);
      setOutput(`${res.stdout}\n${res.stderr}`.trim() || null);
      if (!res.success) {
        setState("failed");
        setError(`Install exited with code ${res.exit_code ?? "unknown"}.`);
        return;
      }
      setRecovered(res.recovered_sessions);
      setState("idle");
      await respawn();
    } catch (e) {
      setState("failed");
      setError(installFailureMessage(e));
    }
  };

  return { state, error, output, recovered, install };
}

// The event names the agent ("Failed to start structured view agent \"x\": ...");
// a failed `/acp/spawn` answers "spawn failed: ...", sometimes after a reason code.
const STARTUP_ERROR_PREFIX =
  /^(?:Failed to start structured view agent "[^"]*":\s*|(?:worker_not_ready|rate_limited|spawn_cancelled):\s*)?(?:spawn failed:\s*)?/;

/** A startup error's own text, without the prefix that names where it came from. */
export function startupErrorCause(message: string): string {
  return message.trim().replace(STARTUP_ERROR_PREFIX, "").trim();
}

/** An install hint split into the command to copy and its trailing note, such
 *  as OpenCode's "(then `opencode acp`)", which isn't part of the command. */
export function splitInstallHint(hint: string): { command: string; note: string | null } {
  const m = /^(.*?)\s+\((.+)\)$/s.exec(hint.trim());
  return m ? { command: m[1]!, note: m[2]! } : { command: hint.trim(), note: null };
}
