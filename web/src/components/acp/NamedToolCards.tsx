/* eslint-disable react-refresh/only-export-components */
// Cards for tools recognised by name or payload rather than ACP kind: MCP,
// memory, skills, and Claude's scheduling and harness tools.

import { useMemo } from "react";
import DOMPurify from "dompurify";
import { marked } from "marked";
import {
  Activity,
  Brain,
  Calendar,
  CalendarPlus,
  CalendarX,
  Clock,
  Plug,
  Search,
  Sparkles,
  Square,
} from "lucide-react";

import { useSkillIndex } from "../../hooks/useSkillIndex";
import { parseJsonObject, pickStr } from "../../lib/acpArgs";
import type { ToolCall } from "../../lib/acpTypes";
import type { AgentProfile } from "../../lib/agentProfiles";
import { humanizeServer, humanizeVerb } from "../../lib/mcpClassify";
import { cleanRecalledMemory, parseMemoryFrontmatter, type MemoryHit } from "../../lib/memoryClassify";
import { badgeLabel, badgeTone, resolveSkillSource } from "../../lib/skillProvenance";
import { ProvenanceBadge } from "../ProvenanceBadge";
import { GenericToolCard } from "./CoreToolCards";
import {
  CardChrome,
  HighlightedBlock,
  RawBlock,
  formatDurationSeconds,
  isAcpBookkeepingKey,
  statusFor,
  useInputJson,
  useToolArgs,
  useToolCardExpansion,
  type ToolCardProps,
} from "./ToolCardChrome";
import { ToolErrorBody } from "./ToolErrorBody";

const ICON = "h-3.5 w-3.5";
const TABULAR_META = "hidden md:inline text-[11px] text-text-dim tabular-nums";

const hasUserArgs = (args: Record<string, unknown> | null) =>
  Boolean(args && Object.keys(args).filter((k) => !isAcpBookkeepingKey(k)).length > 0);

/** MCP and skill cards: header plus the copyable input and highlighted output. */
function ArgsCard({
  tool,
  result,
  icon,
  label,
  primary,
  meta,
  outputMaxLines,
}: ToolCardProps & {
  icon: React.ReactNode;
  label: string;
  primary: React.ReactNode;
  meta?: React.ReactNode;
  outputMaxLines: number;
}) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);
  const args = useToolArgs(tool);
  const inputJson = useInputJson(tool, args);
  const output = result?.text ?? "";
  const hasBody = Boolean((args && Object.keys(args).length > 0) || output);
  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={icon}
      label={label}
      primary={primary}
      meta={meta}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasUserArgs(args) && <RawBlock label="input" text={inputJson} />}
          {output && status !== "err" && (
            <HighlightedBlock text={output} language="markdown" maxLines={outputMaxLines} />
          )}
        </ToolErrorBody>
      }
    />
  );
}

export function McpToolCard({ tool, result, server, verb }: ToolCardProps & { server: string; verb: string }) {
  const args = useToolArgs(tool);
  // First non-empty string arg, capped, as a header hint.
  const argPreview = useMemo<string | null>(() => {
    for (const [k, v] of Object.entries(args ?? {})) {
      if (isAcpBookkeepingKey(k)) continue;
      if (typeof v === "string" && v.length > 0) return `${k}: ${v.length > 120 ? `${v.slice(0, 117)}…` : v}`;
    }
    return null;
  }, [args]);
  return (
    <ArgsCard
      tool={tool}
      result={result}
      icon={<Plug className={ICON} />}
      label={`MCP · ${humanizeServer(server)}`}
      primary={
        <>
          {humanizeVerb(verb)}
          {argPreview && <span className="ml-2 text-text-dim">· {argPreview}</span>}
        </>
      }
      outputMaxLines={24}
    />
  );
}

/** Claude's Skill tool arrives as `kind: "other"` with the skill id hidden in args. */
export function classifySkill(
  tool: ToolCall,
  profile: AgentProfile,
): { isSkill: true; name: string } | { isSkill: false } {
  if (tool.kind !== "other") return { isSkill: false };
  const title = tool.name?.trim().toLowerCase() ?? "";
  if (!profile.specialTitles.skillNames.includes(title)) return { isSkill: false };
  const name = pickStr(parseJsonObject(tool.args_preview), "skill", "name", "skill_name") ?? "skill";
  return { isSkill: true, name };
}

export function SkillToolCard({ tool, result, skillName }: ToolCardProps & { skillName: string }) {
  // Same index as the composer picker and skills manager, so badges agree everywhere.
  const skillIndex = useSkillIndex();
  const skillSource = useMemo(() => resolveSkillSource(skillIndex, skillName), [skillIndex, skillName]);
  return (
    <ArgsCard
      tool={tool}
      result={result}
      icon={<Sparkles className={ICON} />}
      label="skill"
      primary={skillName}
      meta={skillSource && <ProvenanceBadge label={badgeLabel(skillSource)} tone={badgeTone(skillSource)} />}
      outputMaxLines={16}
    />
  );
}

const FRONTMATTER_FIELDS = ["name", "type", "description"] as const;

/** Claude memory files touched through plain Read/Edit/Write. */
export function MemoryCard({ tool, result, hit }: ToolCardProps & { hit: MemoryHit }) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);
  const args = useToolArgs(tool);
  const content = useMemo<string>(
    () =>
      hit.verb === "recalled"
        ? (result?.text ?? "")
        : (pickStr(args, "new_string", "newString", "new_str", "content") ?? ""),
    [hit.verb, args, result?.text],
  );
  const parsed = useMemo(() => (content ? parseMemoryFrontmatter(content) : null), [content]);
  const hasBody = Boolean(content);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Brain className={ICON} />}
      label={hit.isIndex ? "Memory index" : "Memory"}
      primary={
        <>
          <span>{hit.isIndex && hit.verb === "recalled" ? "read index" : hit.verb}</span>
          <span className="ml-2 text-text-dim">· {hit.basename}</span>
        </>
      }
      meta={parsed?.type && <span className="hidden md:inline text-[11px] text-text-dim">{parsed.type}</span>}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasBody && parsed && status !== "err" ? (
            <div className="border-t border-surface-800 bg-surface-950">
              {(parsed.name || parsed.description || parsed.type) && (
                <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 px-3 py-2 text-[11px]">
                  {FRONTMATTER_FIELDS.map(
                    (field) => parsed[field] && <FrontmatterRow key={field} field={field} value={parsed[field]} />,
                  )}
                </dl>
              )}
              {parsed.body && <HighlightedBlock text={parsed.body} language="markdown" maxLines={24} />}
            </div>
          ) : null}
        </ToolErrorBody>
      }
    />
  );
}

function FrontmatterRow({ field, value }: { field: string; value: string }) {
  return (
    <>
      <dt className="text-text-dim">{field}</dt>
      <dd className="text-text-secondary">{value}</dd>
    </>
  );
}

/** Session-start memory load (claude-agent-acp `memory_recall`): loaded paths, or a synthesized summary. */
export function MemoryRecallCard({ tool, result }: ToolCardProps) {
  const status = statusFor(result);
  const recall = tool.memory_recall;
  const [open, setOpen] = useToolCardExpansion(status);

  if (!recall) return <GenericToolCard tool={tool} result={result} />;
  const paths = recall.paths ?? [];
  const synthesized = cleanRecalledMemory(recall.synthesized_text ?? "");
  const isSynthesize = recall.mode === "synthesize";
  // `marked`, not <Markdown>, which needs the assistant-ui runtime. The text is
  // agent-surfaced and marked does not sanitize, so DOMPurify is mandatory.
  const synthesizedHtml = isSynthesize ? DOMPurify.sanitize(marked.parse(synthesized, { async: false }) as string) : "";
  const hasBody = isSynthesize ? synthesized.length > 0 : paths.length > 0;

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Brain className={ICON} />}
      label="Memory recall"
      primary={
        isSynthesize ? (
          <span>Synthesised memory</span>
        ) : (
          <>
            <span>Recalled</span>
            <span className="ml-2 text-text-dim">
              · {paths.length} {paths.length === 1 ? "memory" : "memories"}
            </span>
          </>
        )
      }
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {status !== "err" && hasBody ? (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              {isSynthesize ? (
                <div
                  data-testid="memory-recall-synthesized"
                  className="acp-markdown break-words text-[11px] text-text-secondary"
                  dangerouslySetInnerHTML={{ __html: synthesizedHtml }}
                />
              ) : (
                <ul data-testid="memory-recall-paths" className="space-y-0.5 text-[11px] text-text-secondary">
                  {paths.map((p) => (
                    <li key={p} className="break-all font-mono">
                      {p}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          ) : null}
        </ToolErrorBody>
      }
    />
  );
}

type Args = Record<string, unknown> | null;
interface SpecialHeader {
  icon: React.ReactNode;
  label: string;
  primary: React.ReactNode;
  meta?: React.ReactNode;
}

const numberArg = (raw: unknown) => (typeof raw === "number" ? raw : typeof raw === "string" ? Number(raw) : NaN);
const mono = (text: string | null, fallback: string) => (text ? <span className="font-mono">{text}</span> : fallback);

function withReason(main: React.ReactNode, reason: string | null) {
  return (
    <span>
      {main}
      {reason ? <span className="text-text-dim">: {reason}</span> : null}
    </span>
  );
}

/** Claude scheduling and agent-runtime tools, name-matched because the adapter
 *  sends them as `kind: "other"`. Titles map to header builders. */
const SPECIAL_TOOLS = {
  ScheduleWakeup: (tool: ToolCall, args: Args): SpecialHeader => {
    const delaySeconds = numberArg(args ? args["delaySeconds"] : undefined);
    const started = Date.parse(tool.started_at);
    const wakeAt =
      Number.isFinite(started) && Number.isFinite(delaySeconds) ? new Date(started + delaySeconds * 1000) : null;
    const pad = (n: number) => String(n).padStart(2, "0");
    return {
      icon: <Clock className={ICON} />,
      label: "scheduled wakeup",
      primary: withReason(
        Number.isFinite(delaySeconds) ? `in ${formatDurationSeconds(delaySeconds)}` : "scheduled",
        pickStr(args, "reason"),
      ),
      meta: wakeAt && (
        <span className={TABULAR_META}>
          wakes at {pad(wakeAt.getHours())}:{pad(wakeAt.getMinutes())}
        </span>
      ),
    };
  },
  CronCreate: (_tool: ToolCall, args: Args): SpecialHeader => ({
    icon: <CalendarPlus className={ICON} />,
    label: "cron schedule created",
    primary: withReason(
      mono(pickStr(args, "schedule", "cron", "expression"), "schedule created"),
      pickStr(args, "reason"),
    ),
  }),
  CronList: (): SpecialHeader => ({
    icon: <Calendar className={ICON} />,
    label: "cron schedules",
    primary: "list active schedules",
  }),
  CronDelete: (_tool: ToolCall, args: Args): SpecialHeader => ({
    icon: <CalendarX className={ICON} />,
    label: "cron schedule deleted",
    primary: mono(pickStr(args, "id", "name"), "deleted"),
  }),
  ToolSearch: (_tool: ToolCall, args: Args): SpecialHeader => ({
    icon: <Search className={ICON} />,
    label: "tool search",
    primary: mono(pickStr(args, "query"), "search tools"),
  }),
  Monitor: (_tool: ToolCall, args: Args): SpecialHeader => {
    const description = pickStr(args, "description");
    const timeoutMs = numberArg(args ? args["timeout_ms"] : undefined);
    const chip =
      (args ? args["persistent"] : undefined) === true
        ? "persistent"
        : Number.isFinite(timeoutMs)
          ? formatDurationSeconds(timeoutMs / 1000)
          : null;
    return {
      icon: <Activity className={ICON} />,
      label: "monitor",
      primary: description ? <span>{description}</span> : "background watch",
      meta: chip && <span className={TABULAR_META}>{chip}</span>,
    };
  },
  TaskStop: (_tool: ToolCall, args: Args): SpecialHeader => ({
    icon: <Square className={ICON} />,
    label: "task stop",
    primary: mono(pickStr(args, "task_id", "shell_id"), "stop task"),
  }),
};

export type SpecialToolName = keyof typeof SPECIAL_TOOLS;

export const SCHEDULE_TOOLS: readonly SpecialToolName[] = ["ScheduleWakeup", "CronCreate", "CronList", "CronDelete"];
export const HARNESS_TOOLS: readonly SpecialToolName[] = ["ToolSearch", "Monitor", "TaskStop"];

/** The `family` tool this call is, when its title is also in the profile's `allowed` list. */
export function classifySpecialTool(
  tool: ToolCall,
  allowed: readonly string[],
  family: readonly SpecialToolName[],
): SpecialToolName | null {
  if (tool.kind !== "other") return null;
  const title = (pickStr(parseJsonObject(tool.args_preview), "_aoe_title") ?? tool.name ?? "").trim();
  return allowed.includes(title) && family.includes(title as SpecialToolName) ? (title as SpecialToolName) : null;
}

export function SpecialToolCard({ tool, result, name }: ToolCardProps & { name: SpecialToolName }) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);
  const args = useToolArgs(tool);
  const output = result?.text ?? "";
  // A wakeup's `prompt` is a loop sentinel or a repeat of the user's input.
  const omit = name === "ScheduleWakeup" ? "prompt" : undefined;
  const inputJson = useInputJson(tool, args, omit);
  const hasRawInput = args
    ? Object.keys(args).some((k) => !isAcpBookkeepingKey(k) && k !== omit)
    : Boolean(tool.args_preview);
  const header = SPECIAL_TOOLS[name](tool, args);
  const hasBody = hasRawInput || Boolean(output) || status === "err";

  return (
    <CardChrome
      status={status}
      {...header}
      expanded={open}
      onToggle={hasBody ? () => setOpen((v) => !v) : undefined}
      startedAt={tool.started_at}
      endedAt={result?.at}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasRawInput && <RawBlock label="input" text={inputJson} />}
          {output && status !== "err" && <RawBlock label="output" text={output} />}
        </ToolErrorBody>
      }
    />
  );
}
