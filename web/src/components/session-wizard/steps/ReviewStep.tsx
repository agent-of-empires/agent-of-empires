import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import type { StepDef, StepId } from "../StepIndicator";
import { getReviewSummary } from "../sessionNames";
import { useServerDown, OFFLINE_TITLE } from "../../../lib/connectionState";
import type { AgentInfo } from "../../../lib/types";
import { isAcpCapable } from "../../../lib/acpCapableTools";

interface WizardData { path: string; title: string; worktreeBranch: string; useWorktree: boolean; attachExisting: boolean; baseBranch: string; group: string; tool: string; profile: string; profileDirty: boolean; yoloMode: boolean; sandboxEnabled: boolean; sandboxImage: string; extraArgs: string; customInstruction: string; commandOverride: string; scratch: boolean; useCockpit: boolean; [key: string]: unknown; }
interface Props { data: WizardData; onChange: (field: string, value: unknown) => void; agents: AgentInfo[]; isSubmitting: boolean; error: string | null; onSubmit: () => void; onJumpTo: (stepId: StepId) => void; steps: StepDef[]; cockpitMasterEnabled: boolean; }

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.userAgent);

function Row({ label, value, stepId, onJumpTo, accent }: { label: string; value: ReactNode; stepId?: StepId; onJumpTo?: (id: StepId) => void; accent?: boolean }) {
  const interactive = stepId && onJumpTo;
  return (
    <button
      type="button"
      onClick={() => interactive && onJumpTo(stepId)}
      disabled={!interactive}
      className={`flex justify-between items-center w-full py-3 border-b border-surface-800 last:border-0 text-left ${
        interactive ? "cursor-pointer hover:bg-surface-800/50 -mx-2 px-2 rounded-md" : "-mx-2 px-2"
      }`}
    >
      <span className="text-sm text-text-dim">{label}</span>
      <span className={`text-sm font-mono truncate ml-4 ${accent ? "text-accent-600" : "text-text-primary"}`}>{value}</span>
    </button>
  );
}

function AgentReviewValue({ name, custom }: { name: string; custom: boolean }) {
  if (!custom) return <>{name}</>;
  return (
    <span className="inline-flex items-center gap-2">
      <span>{name}</span>
      <span className="rounded px-1.5 py-px text-[10px] font-mono uppercase tracking-wide bg-surface-700 text-text-dim">
        Custom
      </span>
    </span>
  );
}

function EditableRow({ label, value, displayValue, placeholder, onChange, accent }: {
  label: string;
  value: string;
  displayValue: string;
  placeholder?: string;
  onChange: (v: string) => void;
  accent?: boolean;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (editing) inputRef.current?.select();
  }, [editing]);

  const startEditing = () => {
    setDraft(value);
    setEditing(true);
  };

  const commit = () => {
    setEditing(false);
    if (draft !== value) onChange(draft);
  };

  const isPlaceholder = !value;

  if (editing) {
    return (
      <div className="flex justify-between items-center w-full py-3 border-b border-surface-800 last:border-0 -mx-2 px-2 gap-3">
        <span className="text-sm text-text-dim shrink-0">{label}</span>
        <input
          ref={inputRef}
          type="text"
          value={draft}
          placeholder={placeholder}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              // Stop the wizard's window-level Cmd+Enter submit handler
              // from racing with our state update on commit.
              e.preventDefault();
              e.stopPropagation();
              commit();
            } else if (e.key === "Escape") {
              e.preventDefault();
              setEditing(false);
            }
          }}
          className={`flex-1 min-w-0 text-sm font-mono bg-surface-800 border border-brand-600 rounded px-2 py-1 text-right placeholder:text-text-dim focus:outline-none ${accent ? "text-accent-600" : "text-text-primary"}`}
        />
      </div>
    );
  }

  return (
    <button
      type="button"
      onClick={startEditing}
      className="flex justify-between items-center w-full py-3 border-b border-surface-800 last:border-0 text-left cursor-pointer hover:bg-surface-800/50 -mx-2 px-2 rounded-md"
    >
      <span className="text-sm text-text-dim">{label}</span>
      <span className={`text-sm font-mono truncate ml-4 ${isPlaceholder ? "text-text-dim italic" : accent ? "text-accent-600" : "text-text-primary"}`}>{displayValue}</span>
    </button>
  );
}

export function ReviewStep({ data, onChange, agents, isSubmitting, error, onSubmit, onJumpTo, steps, cockpitMasterEnabled }: Props) {
  const hasStep = (id: StepId) => steps.some((s) => s.id === id);
  const offline = useServerDown();
  // Scratch sessions intentionally carry no path until the server
  // provisions one on submit; treat that as satisfying the "need a
  // project" gate so the user can launch.
  const canSubmit =
    !isSubmitting && !offline && (data.scratch || !!data.path) && !!data.tool;
  const summary = getReviewSummary(data.title, data.worktreeBranch);
  const selectedAgent = agents.find((agent) => agent.name === data.tool);
  const selectedCustomAgent = selectedAgent?.kind === "custom";
  // Mirror the submit-path computation in SessionWizard.handleSubmit so
  // the review reflects the substrate the session will actually launch
  // with, including the per-session opt-out (#1580).
  const willUseCockpit =
    cockpitMasterEnabled &&
    isAcpCapable(data.tool, selectedAgent?.acp_capable) &&
    data.useCockpit;

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && canSubmit) {
        e.preventDefault();
        onSubmit();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [canSubmit, onSubmit]);

  return (
    <div>
      <h2 className="text-lg font-semibold text-text-primary mb-1">Review & Launch</h2>
      <p className="text-sm text-text-muted mb-5">Here's what will be created. Make sure everything looks right.</p>
      <div className="bg-surface-900 border border-surface-700 rounded-lg p-4 mb-5">
        <Row
          label="Project"
          value={
            data.scratch
              ? "Scratch directory (provisioned on create)"
              : data.path || "(not set)"
          }
          stepId="project"
          onJumpTo={onJumpTo}
        />
        <EditableRow
          label="Title"
          value={data.title}
          displayValue={summary.title}
          placeholder="Auto-generated"
          onChange={(v) => onChange("title", v)}
        />
        {!data.scratch && data.useWorktree ? (
          <>
            <EditableRow
              label="Branch / worktree"
              value={data.worktreeBranch}
              displayValue={summary.branch}
              placeholder="Auto-generated"
              onChange={(v) => onChange("worktreeBranch", v)}
              accent
            />
            <Row
              label="Mode"
              value={data.attachExisting ? "Attach to existing branch" : "Create new branch"}
            />
            {!data.attachExisting && data.baseBranch.trim() && (
              <Row label="Base branch" value={data.baseBranch.trim()} />
            )}
          </>
        ) : data.scratch ? (
          <Row label="Worktree" value="Not applicable (scratch session)" />
        ) : (
          <Row label="Worktree" value="None, runs in repo folder" />
        )}
        <Row
          label="Agent"
          value={<AgentReviewValue name={data.tool || "(not set)"} custom={selectedCustomAgent} />}
          stepId="agent"
          onJumpTo={onJumpTo}
        />
        {cockpitMasterEnabled && (
          <Row label="Interface" value={willUseCockpit ? "Cockpit" : "Terminal"} stepId="agent" onJumpTo={onJumpTo} />
        )}
        {data.profile && (
          <Row label="Profile" value={data.profileDirty ? `${data.profile} (Custom)` : data.profile} stepId="agent" onJumpTo={onJumpTo} accent />
        )}
        {data.sandboxEnabled && (
          <Row label="Container" value={data.sandboxImage || "default"} stepId={hasStep("container") ? "container" : undefined} onJumpTo={onJumpTo} />
        )}
        <Row label="Auto-approve" value={data.yoloMode ? "On" : "Off"} stepId="agent" onJumpTo={onJumpTo} />
        {data.group && <Row label="Group" value={data.group} />}
        {data.extraArgs && <Row label="Extra args" value={data.extraArgs} />}
        {data.customInstruction && <Row label="Instructions" value="(set)" />}
        {data.commandOverride && <Row label="Command override" value={data.commandOverride} />}
      </div>
      {error && <div className="text-sm text-red-400 bg-red-400/10 rounded-lg p-3 mb-4">{error}</div>}
      {offline && (
        <div className="text-sm text-status-error bg-status-error/10 rounded-lg p-3 mb-4">
          {OFFLINE_TITLE}
        </div>
      )}
      <button
        onClick={onSubmit}
        disabled={!canSubmit}
        className={`w-full py-3 rounded-lg font-semibold text-sm transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-green-500 ${
          !canSubmit
            ? "bg-green-500/50 text-surface-900/50 cursor-not-allowed"
            : "bg-green-500 hover:bg-green-600 active:bg-green-700 text-surface-900 cursor-pointer"
        }`}
      >
        {isSubmitting ? (
          <span className="flex items-center justify-center gap-2">
            <svg className="animate-spin h-4 w-4" viewBox="0 0 24 24"><circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" /><path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" /></svg>
            Creating session...
          </span>
        ) : (
          <span>Launch session <span className="opacity-60">({isMac ? "\u2318" : "Ctrl"}+Enter)</span></span>
        )}
      </button>
    </div>
  );
}
