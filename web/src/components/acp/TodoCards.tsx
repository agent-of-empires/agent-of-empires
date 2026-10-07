/* eslint-disable react-refresh/only-export-components */
import { useMemo, useState } from "react";
import { ListChecks } from "lucide-react";

import { parseJsonObject, todoItemsFromArgs } from "../../lib/acpArgs";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";
import { useAgentProfile } from "../../lib/agentProfileContext";
import type { AgentProfile } from "../../lib/agentProfiles";
import {
  CardChrome,
  spanTimes,
  statusFor,
  useToolCardExpansion,
  type Status,
  type ToolCardProps,
} from "./ToolCardChrome";
import { ToolErrorBody } from "./ToolErrorBody";

type TodoStatus = "pending" | "in_progress" | "completed" | "cancelled";

interface TodoItem {
  content: string;
  status: TodoStatus;
}

/** Agent todo tools carry the full list in `args.todos`; the profile opts in.
 *  An empty `todos: []` is a real clear, so the gate is array presence. */
export function classifyTodoWrite(
  tool: ToolCall,
  profile: AgentProfile,
): { isTodoWrite: true; todos: TodoItem[] } | { isTodoWrite: false } {
  if (!profile.capabilities.todos) return { isTodoWrite: false };
  const args = parseJsonObject(tool.args_preview);
  if (!Array.isArray(args?.todos)) return { isTodoWrite: false };
  const todos = todoItemsFromArgs(args).map((entry) => ({
    content: entry.content,
    status: normaliseTodoStatus(entry.status),
  }));
  return { isTodoWrite: true, todos };
}

const STATUS_ALIASES: Record<string, TodoStatus> = {
  in_progress: "in_progress",
  "in-progress": "in_progress",
  active: "in_progress",
  completed: "completed",
  complete: "completed",
  done: "completed",
  cancelled: "cancelled",
  canceled: "cancelled",
  abandoned: "cancelled",
};

function normaliseTodoStatus(raw: unknown): TodoStatus {
  const s = typeof raw === "string" ? raw.toLowerCase() : "";
  return Object.hasOwn(STATUS_ALIASES, s) ? STATUS_ALIASES[s]! : "pending";
}

const TODO_STYLE: Record<TodoStatus, [glyph: string, className: string]> = {
  pending: ["☐", "text-text-secondary"],
  in_progress: ["▶", "text-brand-400"],
  completed: ["✓", "text-emerald-400 line-through opacity-70"],
  cancelled: ["⊘", "text-text-dim line-through"],
};

const BREAKDOWN_ORDER: [TodoStatus, string][] = [
  ["in_progress", "active"],
  ["pending", "pending"],
  ["completed", "done"],
  ["cancelled", "cancelled"],
];

function todoBreakdown(todos: TodoItem[]): string[] {
  return BREAKDOWN_ORDER.flatMap(([status, word]) => {
    const n = todos.filter((t) => t.status === status).length;
    return n > 0 ? [`${n} ${word}`] : [];
  });
}

function Breakdown({ todos }: { todos: TodoItem[] }) {
  const breakdown = todoBreakdown(todos);
  return breakdown.length > 0 && <span className="ml-2 text-text-dim">· {breakdown.join(" · ")}</span>;
}

function TodoList({ todos }: { todos: TodoItem[] }) {
  if (todos.length === 0) {
    return (
      <div className="border-t border-surface-800 bg-surface-950 px-3 py-2 font-mono text-xs text-text-dim">
        todos cleared
      </div>
    );
  }
  return (
    <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
      <ul className="flex flex-col gap-1 font-mono text-xs">
        {todos.map((t, i) => (
          <li key={`${i}-${t.content}`} className={`flex items-start gap-2 ${TODO_STYLE[t.status][1]}`}>
            <span className="select-none w-4 shrink-0 text-center">{TODO_STYLE[t.status][0]}</span>
            <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{t.content}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

export function TodoUpdateCard({ tool, result, todos }: ToolCardProps & { todos: TodoItem[] }) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status, todos.length <= 5);
  return (
    <CardChrome
      status={status}
      icon={<ListChecks className="h-3.5 w-3.5" />}
      label="todos"
      primary={
        <>
          <span>{todos.length === 0 ? "todos cleared" : `${todos.length} items`}</span>
          <Breakdown todos={todos} />
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      startedAt={tool.started_at}
      endedAt={result?.at}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          <TodoList todos={todos} />
        </ToolErrorBody>
      }
    />
  );
}

/** A run of todo snapshots folded into one card: the latest list stays visible,
 *  each update is behind the expand toggle. */
export function TodoGroupCard({ items }: { items: ToolCardProps[] }) {
  const profile = useAgentProfile();
  const [open, setOpen] = useState(false);
  const snapshots = useMemo(
    () =>
      items.flatMap((it) => {
        const c = classifyTodoWrite(it.tool, profile);
        return c.isTodoWrite ? [{ tool: it.tool, result: it.result, todos: c.todos }] : [];
      }),
    [items, profile],
  );
  if (snapshots.length === 0) return null;

  // Preview the latest snapshot that became live state; the header reflects the latest attempt.
  const latestAttempt = snapshots[snapshots.length - 1]!;
  const failedOrStopped = (r?: ActivityRow) => r?.kind === "tool_error" || r?.kind === "tool_stopped";
  const preview = [...snapshots].reverse().find((s) => !failedOrStopped(s.result)) ?? latestAttempt;
  const latestFailed = latestAttempt.result?.kind === "tool_error";
  const latestStopped = latestAttempt.result?.kind === "tool_stopped";
  const running = snapshots.some((s) => !s.result);
  const status: Status = running ? "running" : latestFailed ? "err" : latestStopped ? "stopped" : "ok";

  return (
    <CardChrome
      status={status}
      neutralOnDone={!latestFailed && !latestStopped}
      icon={<ListChecks className="h-3.5 w-3.5" />}
      label="todos"
      primary={
        <>
          <span>updated {snapshots.length} times</span>
          <Breakdown todos={preview.todos} />
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      {...spanTimes(snapshots)}
      subBody={preview.result?.kind === "tool_error" ? undefined : <TodoList todos={preview.todos} />}
      body={
        <div className="border-t border-surface-800 bg-surface-900/30 px-2 py-1">
          {snapshots.map((s) => (
            <TodoUpdateCard key={s.tool.id} tool={s.tool} result={s.result} todos={s.todos} />
          ))}
        </div>
      }
    />
  );
}
