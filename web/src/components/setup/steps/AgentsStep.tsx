import { RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import { fetchProfileAgents } from "../../../lib/api";
import type { AgentInfo } from "../../../lib/types";
import { CopyCommand, GhostButton, PANEL, RadioRow, Section } from "../ui";

/** Shown with their install command when missing, so a fresh machine has somewhere to start. */
const SUGGESTED = ["claude", "codex", "gemini"];

interface Props {
  profile: string;
  defaultTool: string | undefined;
  structuredView: string | undefined;
  save: (section: string, field: string, value: unknown) => Promise<boolean>;
  onAgentsChecked: () => void;
}

export function AgentsStep({ profile, defaultTool, structuredView, save, onAgentsChecked }: Props) {
  const [agents, setAgents] = useState<AgentInfo[] | null>(null);
  const [checking, setChecking] = useState(true);

  const [checks, setChecks] = useState(0);

  useEffect(() => {
    let live = true;
    void fetchProfileAgents(profile || undefined).then((result) => {
      if (!live) return;
      setAgents(Array.isArray(result) ? result : []);
      setChecking(false);
      onAgentsChecked();
    });
    return () => {
      live = false;
    };
  }, [profile, onAgentsChecked, checks]);
  const check = () => {
    setChecking(true);
    setChecks((n) => n + 1);
  };

  const installed = (agents ?? []).filter((a) => a.installed);
  const missing = (agents ?? []).filter((a) => !a.installed && SUGGESTED.includes(a.name));
  const [tool, setTool] = useState(defaultTool);
  const [view, setView] = useState(structuredView === "terminal" ? "terminal" : "structured");
  const chosen = tool && installed.some((a) => a.name === tool) ? tool : installed[0]?.name;

  const pickTool = async (name: string) => {
    setTool(name);
    if (!(await save("session", "default_tool", name))) setTool(tool);
  };
  const pickView = async (next: string) => {
    setView(next);
    if (!(await save("acp", "structured_view", next))) setView(view);
  };

  return (
    <>
      <Section
        title="Detected on this machine"
        hint="Looked for Agents on the Daemon's PATH."
        action={
          <GhostButton onClick={check} disabled={checking}>
            <span className="inline-flex items-center gap-1.5">
              <RefreshCw size={12} className={checking ? "animate-spin" : undefined} />
              {checking ? "Checking" : "Check again"}
            </span>
          </GhostButton>
        }
      >
        <div className={PANEL}>
          {agents === null && <div className="px-4 py-3 text-sm text-text-dim">Checking for Agents…</div>}
          {agents !== null && installed.length === 0 && (
            <div className="px-4 py-3 text-sm text-status-warning">No Agents found. Install one, then check again.</div>
          )}
          {installed.map((agent) => (
            <div key={agent.name} className="px-4 py-2.5 flex flex-col gap-2">
              <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
                <span className="h-2 w-2 rounded-full bg-status-running" aria-hidden="true" />
                <span className="font-mono text-sm text-text-primary w-24">{agent.name}</span>
                <span className="text-sm text-text-secondary">{agent.display_name}</span>
                {agent.path && <span className="font-mono text-xs text-text-dim ml-auto truncate">{agent.path}</span>}
                {agent.acp_capable &&
                  (agent.acp_installed ? (
                    <span className="text-xs rounded border border-status-running/50 text-status-running px-1.5 py-0.5">
                      Structured view ready
                    </span>
                  ) : (
                    <span className="text-xs rounded border border-status-warning/50 text-status-warning px-1.5 py-0.5">
                      Adapter missing
                    </span>
                  ))}
              </div>
              {agent.acp_capable && !agent.acp_installed && agent.acp_install_command && (
                <>
                  <p className="text-xs text-text-dim">
                    Terminal view works now. Install the adapter for Structured view.
                  </p>
                  <CopyCommand command={agent.acp_install_command} />
                </>
              )}
            </div>
          ))}
          {missing.map((agent) => (
            <div key={agent.name} className="px-4 py-2.5 flex flex-col gap-2">
              <div className="flex items-center gap-4">
                <span className="h-2 w-2 rounded-full border border-text-dim" aria-hidden="true" />
                <span className="font-mono text-sm text-text-primary w-24">{agent.name}</span>
                <span className="text-sm text-text-secondary">{agent.display_name}</span>
                <span className="text-xs text-text-dim">Not installed</span>
              </div>
              <CopyCommand command={agent.install_hint} />
            </div>
          ))}
        </div>
      </Section>
      {installed.length > 0 && (
        <>
          <Section title="Default Agent" hint="New Sessions start with it. You can still pick another per Session.">
            <div className={PANEL}>
              {installed.map((agent) => (
                <RadioRow
                  key={agent.name}
                  name="default-agent"
                  checked={chosen === agent.name}
                  onSelect={() => void pickTool(agent.name)}
                >
                  <span className="font-mono text-sm text-text-primary">{agent.name}</span>
                  <span className="ml-4 text-sm text-text-dim">{agent.display_name}</span>
                </RadioRow>
              ))}
            </div>
          </Section>
          <Section title="Default view" hint="How a Session opens. Switch any time from the Session header.">
            <div className={PANEL}>
              <RadioRow
                name="default-view"
                checked={view === "structured"}
                onSelect={() => void pickView("structured")}
              >
                <div className="text-sm text-text-primary">Structured view</div>
                <div className="text-xs text-text-dim">Chat-style turns with tool calls, diffs and approvals.</div>
              </RadioRow>
              <RadioRow name="default-view" checked={view === "terminal"} onSelect={() => void pickView("terminal")}>
                <div className="text-sm text-text-primary">Terminal view</div>
                <div className="text-xs text-text-dim">The Agent in its raw terminal, as in tmux.</div>
              </RadioRow>
            </div>
          </Section>
        </>
      )}
    </>
  );
}
