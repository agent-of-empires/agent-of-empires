// One sub-agent as Input, Activity and Output, the same in the thread, the pane and on mobile.

import { ChevronDown, Sparkles } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";

import { CopyMarkdownButton } from "../CopyMarkdownButton";
import { formatDurationSeconds, useToolCardExpansion, type Status } from "../ToolCardChrome";
import { useBackgroundAgentsContextValue } from "../backgroundAgentsContext";
import { ActivityFilters, ActivityRows } from "./ActivitySection";
import type { ActivityFilter } from "./activityFold";
import { PreviewMarkdown, SteppedMarkdown } from "./LongMarkdown";
import type { SubagentRecord, SubagentStatus } from "./model";
import { StopSession } from "./StopSession";

const STATUS: Record<SubagentStatus, { glyph: string; word: string; dot: string; tone: string }> = {
  running: { glyph: "●", word: "running", dot: "bg-status-waiting animate-pulse", tone: "text-text-dim" },
  waiting: { glyph: "●", word: "Waiting on you", dot: "bg-status-waiting animate-pulse", tone: "text-text-dim" },
  done: { glyph: "✓", word: "done", dot: "bg-status-running", tone: "text-text-dim" },
  failed: { glyph: "✕", word: "failed", dot: "bg-status-error", tone: "text-status-error" },
  stalled: { glyph: "!", word: "stalled", dot: "bg-text-dim/60", tone: "text-text-dim" },
  detached: { glyph: "○", word: "detached", dot: "bg-text-dim/60", tone: "text-text-dim" },
  unknown: { glyph: "?", word: "Unknown", dot: "bg-text-dim/60", tone: "text-text-dim" },
};

const SECTION_HEAD = "sticky top-0 z-10 flex items-center gap-2 px-3 py-1 text-[10px] uppercase tracking-wider";

function toCardStatus(s: SubagentStatus): Status {
  return s === "running" || s === "waiting" ? "running" : s === "failed" ? "err" : "ok";
}

function useElapsed(record: SubagentRecord): string | null {
  const live = (record.status === "running" || record.status === "waiting") && !record.endedAt;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!live) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [live]);
  if (!record.startedAt) return null;
  const start = Date.parse(record.startedAt);
  const end = record.endedAt ? Date.parse(record.endedAt) : now;
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  return formatDurationSeconds((end - start) / 1000);
}

/** The one fact that changes with status, shown under the header. */
function summaryLine(r: SubagentRecord): string | null {
  const last = r.tools.length < r.toolCount ? undefined : r.tools.at(-1);
  const line = (t: string) => t.split("\n", 1)[0]!.trim();
  if (r.status === "running") {
    if (last) return `${last.name}${last.target ? ` ${last.target}` : ""}`;
    if (r.lastTool) return r.lastTool;
    return r.latestNote ? line(r.latestNote) : null;
  }
  if (r.status === "failed") {
    const failing = r.tools.find((t) => t.status === "err");
    if (failing) return `${failing.name}${failing.target ? ` ${failing.target}` : ""} failed`;
    return r.error ? line(r.error) : null;
  }
  return r.report ? line(r.report) : null;
}

interface Props {
  record: SubagentRecord;
  /** Fixed open, as in the pane and the sheet; the thread card folds. */
  alwaysOpen?: boolean;
  /** Header actions beside the toggle, such as "Open in side pane". */
  actions?: ReactNode;
}

export function SubagentDetail({ record, alwaysOpen = false, actions }: Props) {
  const [toggled, setOpen] = useToolCardExpansion(toCardStatus(record.status));
  const open = alwaysOpen || toggled;
  const uid = useId();
  const { sessionId, agents, readOnly } = useBackgroundAgentsContextValue();
  const elapsed = useElapsed(record);
  const st = STATUS[record.status];
  const errors = record.tools.filter((t) => t.status === "err").length;
  const summary = summaryLine(record);
  const running = agents.filter((a) => a.status === "running").length;
  const bodyRef = useRef<HTMLDivElement>(null);

  return (
    <section
      data-tool-id={record.id}
      data-testid="subagent-detail"
      className="my-1 rounded-md border border-surface-700 bg-surface-800/50 text-sm"
    >
      <div className="flex flex-wrap items-center gap-x-2">
        <button
          type="button"
          aria-expanded={open}
          aria-controls={`${uid}-body`}
          disabled={alwaysOpen}
          onClick={() => setOpen((v) => !v)}
          className="flex min-w-0 flex-1 items-center gap-2 px-3 py-1.5 text-left enabled:hover:bg-surface-800"
        >
          <span className={`h-2 w-2 shrink-0 rounded-full ${st.dot}`} />
          <Sparkles className="h-3.5 w-3.5 shrink-0 text-text-dim" />
          <span className="hidden text-[11px] uppercase tracking-wider text-text-dim min-[480px]:inline">subagent</span>
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-text-secondary">{record.description}</span>
          {record.background && (
            <span className="hidden rounded bg-surface-700 px-1.5 text-[10px] text-text-dim min-[480px]:inline">
              background
            </span>
          )}
          {record.toolCount > 0 && (
            <span className="hidden shrink-0 text-[11px] tabular-nums text-text-dim min-[480px]:inline">
              {record.toolCount} {record.toolCount === 1 ? "tool" : "tools"}
            </span>
          )}
          {record.badge && <span className="shrink-0 text-[11px] text-text-dim">{record.badge}</span>}
          {elapsed && <span className="shrink-0 text-[11px] tabular-nums text-text-dim">{elapsed}</span>}
          <span className={`shrink-0 text-[11px] ${st.tone}`}>
            <span aria-hidden>{st.glyph} </span>
            {st.word}
          </span>
          {!alwaysOpen && (
            <ChevronDown
              className={`h-3.5 w-3.5 shrink-0 text-text-dim transition-transform ${open ? "rotate-180" : ""}`}
            />
          )}
        </button>
        <div className="flex items-center gap-2 pr-2">
          {actions}
          {record.background && record.status === "running" && sessionId && !readOnly && (
            <StopSession sessionId={sessionId} running={running} />
          )}
        </div>
      </div>
      <span className="sr-only" role="status" aria-live="polite">
        {record.description} {st.word}
      </span>
      {summary && !open && <p className="truncate px-3 pb-1.5 pl-9 text-[11px] text-text-dim">{summary}</p>}
      {open && (
        <div id={`${uid}-body`} ref={bodyRef} className="border-t border-surface-800">
          {record.warning && <p className="px-3 py-1 text-[11px] text-status-error">{record.warning}</p>}
          <InputSection record={record} />
          {(record.toolCount > 0 || record.status === "running") && (
            <ActivitySection record={record} errors={errors} bodyRef={bodyRef} scrollToNewest={alwaysOpen} />
          )}
          <OutputSection record={record} />
        </div>
      )}
    </section>
  );
}

function InputSection({ record }: { record: SubagentRecord }) {
  const lines = record.status === "running" ? 1 : 4;
  return (
    <div className="bg-surface-900/50">
      <div className={`${SECTION_HEAD} bg-surface-900 text-text-dim`}>
        <span>Input</span>
        {record.model && <span className="font-mono normal-case">{record.model}</span>}
        {record.prompt && <CopyMarkdownButton compact source={record.prompt} label="Copy prompt" className="ml-auto" />}
      </div>
      <div className="px-3 pb-2 text-xs text-text-secondary">
        {record.prompt ? (
          <PreviewMarkdown text={record.prompt} lines={lines} noun="prompt" />
        ) : (
          <p className="text-text-dim">(no prompt recorded)</p>
        )}
      </div>
    </div>
  );
}

function ActivitySection({
  record,
  errors,
  bodyRef,
  scrollToNewest,
}: {
  record: SubagentRecord;
  errors: number;
  bodyRef: React.RefObject<HTMLDivElement | null>;
  /** The pane and sheet scroll themselves; the thread follows its own transcript. */
  scrollToNewest: boolean;
}) {
  const running = record.status === "running";
  const [expanded, setExpanded] = useState<boolean | null>(null);
  const [following, setFollowing] = useState(true);
  const [filter, setFilter] = useState<ActivityFilter>("all");
  // Done agents keep Activity to its summary row; a running or failed one opens it.
  const open = expanded ?? (running || record.status === "failed");
  const { tools } = record;
  const { toolCount } = record;
  const summary = [
    `${toolCount} ${toolCount === 1 ? "call" : "calls"}`,
    tools.length < toolCount ? `first ${tools.length} shown` : null,
    errors ? `${errors} errors` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  const followed = open && running && following && filter === "all";
  const seenCount = useRef(tools.length);
  // Runs on commit, so only a new call scrolls, not the elapsed-time tick.
  const followNewest = (el: HTMLDivElement | null) => {
    if (!el || seenCount.current === tools.length) return;
    seenCount.current = tools.length;
    if (scrollToNewest && followed) el.lastElementChild?.scrollIntoView?.({ block: "nearest" });
  };
  const jump = () => {
    setExpanded(true);
    setFilter("all");
    setFollowing(false);
    requestAnimationFrame(() =>
      bodyRef.current?.querySelector('[data-activity-status="err"]')?.scrollIntoView({ block: "center" }),
    );
  };
  return (
    <div>
      <div className={`${SECTION_HEAD} bg-surface-800 text-text-dim`}>
        <button type="button" aria-expanded={open} onClick={() => setExpanded(!open)} className="uppercase">
          Activity
        </button>
        <span className="normal-case">{summary}</span>
        {open && running && (
          <button
            type="button"
            aria-pressed={following}
            onClick={() => setFollowing((v) => !v)}
            className="normal-case text-accent-600 hover:underline"
          >
            {following ? "Following" : "Follow"}
          </button>
        )}
        {record.status === "failed" && errors > 0 && (
          <button type="button" onClick={jump} className="normal-case text-status-error hover:underline">
            Jump to failure
          </button>
        )}
        {open && tools.length > 8 && (
          <span className="ml-auto">
            <ActivityFilters tools={tools} filter={filter} onChange={setFilter} />
          </span>
        )}
      </div>
      {open && (
        <div ref={followNewest} className="pb-1">
          <ActivityRows tools={tools} filter={filter} following={running && following} running={running} />
        </div>
      )}
    </div>
  );
}

function OutputSection({ record }: { record: SubagentRecord }) {
  const { status, report, latestNote, error } = record;
  let body: ReactNode;
  if (status === "failed") {
    body = (
      <>
        <pre className="whitespace-pre-wrap break-words font-mono text-[11px] text-status-error">{error}</pre>
        {!record.background && (
          <p className="mt-1 text-[11px] text-text-dim">The main agent gets this error as the Task result.</p>
        )}
      </>
    );
  } else if (status === "running") {
    body = latestNote ? (
      <>
        <p className="mb-1 text-[11px] text-text-dim">Latest note, not the final report</p>
        <p className="whitespace-pre-wrap break-words">{latestNote}</p>
      </>
    ) : (
      <p className="text-text-dim">Running…</p>
    );
  } else if (report) {
    body = <SteppedMarkdown text={report} />;
  } else if (status === "stalled" || status === "detached") {
    body = <p className="text-text-dim">No activity; nothing to stop.</p>;
  } else {
    body = (
      <p className="text-text-dim">
        {record.tools.length > 0
          ? "Finished without writing a report; its tool results are in Activity."
          : "Finished without writing a report."}
      </p>
    );
  }
  return (
    <div className="bg-surface-800">
      <div className={`${SECTION_HEAD} bg-surface-800 text-text-bright`}>
        <span>Output</span>
        {status === "done" && report && (
          <CopyMarkdownButton compact source={report} label="Copy report" className="ml-auto" />
        )}
      </div>
      <div className="px-3 pb-2 text-xs text-text-primary">{body}</div>
    </div>
  );
}
