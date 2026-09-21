// Tool-call card dispatch: picks the purpose-built card for each call.

import type { ReactNode } from "react";

import { parseJsonObject, pickStr } from "../../lib/acpArgs";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";
import { useAgentProfile } from "../../lib/agentProfileContext";
import type { AgentProfile, CardKind } from "../../lib/agentProfiles";
import { classifyMcp } from "../../lib/mcpClassify";
import { classifyMemory } from "../../lib/memoryClassify";
import { reclassifyBash } from "../../lib/toolReclassify";
import {
  DeleteToolCard,
  EditToolCard,
  ExecuteToolCard,
  FetchToolCard,
  GenericToolCard,
  ReadToolCard,
  SearchToolCard,
  ThinkToolCard,
} from "./CoreToolCards";
import {
  HARNESS_TOOLS,
  MemoryCard,
  MemoryRecallCard,
  McpToolCard,
  SCHEDULE_TOOLS,
  SkillToolCard,
  SpecialToolCard,
  classifySkill,
  classifySpecialTool,
} from "./NamedToolCards";
import { classifyTodoWrite, TodoUpdateCard } from "./TodoCards";
import { statusFor } from "./ToolCardChrome";
import { ToolOutputMedia } from "./ToolOutputMedia";

interface Props {
  tool: ToolCall;
  result?: ActivityRow;
  /** Inside a SubagentCard, whose border already shows the linkage, so skip the subagent wrap. */
  nested?: boolean;
}

export function ToolCard({ tool, result, nested }: Props) {
  const profile = useAgentProfile();
  const card = renderToolCard(tool, result, profile);
  const content = result?.output?.length ? (
    <>
      {card}
      <ToolOutputMedia blocks={result.output} />
    </>
  ) : (
    card
  );
  if (!nested && pickStr(parseJsonObject(tool.args_preview), "_aoe_parent_tool_call_id")) {
    return <SubagentChildWrap>{content}</SubagentChildWrap>;
  }
  return content;
}

const KIND_CARDS: Record<string, (props: { tool: ToolCall; result?: ActivityRow }) => ReactNode> = {
  execute: ExecuteToolCard,
  read: ReadToolCard,
  edit: EditToolCard,
  delete: DeleteToolCard,
  fetch: FetchToolCard,
  // A failed think has nothing to show but its error, which only the generic card renders.
  think: (props) => (statusFor(props.result) === "err" ? <GenericToolCard {...props} /> : <ThinkToolCard {...props} />),
};

function renderToolCard(tool: ToolCall, result: ActivityRow | undefined, profile: AgentProfile) {
  // Structured recall wins over the path-sniffing memory classifier.
  if (tool.memory_recall) return <MemoryRecallCard tool={tool} result={result} />;
  const memory = classifyMemory(tool);
  if (memory.isMemory) return <MemoryCard tool={tool} result={result} hit={memory} />;
  const mcp = classifyMcp(tool, profile);
  if (mcp.isMcp) return <McpToolCard tool={tool} result={result} server={mcp.server} verb={mcp.verb} />;
  const { capabilities, specialTitles } = profile;
  if (capabilities.skills) {
    const skill = classifySkill(tool, profile);
    if (skill.isSkill) return <SkillToolCard tool={tool} result={result} skillName={skill.name} />;
  }
  if (capabilities.todos) {
    const todos = classifyTodoWrite(tool, profile);
    if (todos.isTodoWrite) return <TodoUpdateCard tool={tool} result={result} todos={todos.todos} />;
  }
  const special =
    (capabilities.wakeup ? classifySpecialTool(tool, specialTitles.scheduleNames, SCHEDULE_TOOLS) : null) ??
    classifySpecialTool(tool, specialTitles.harnessNames, HARNESS_TOOLS);
  if (special) return <SpecialToolCard tool={tool} result={result} name={special} />;
  const { kind, provenance } = reclassifyBash(tool);
  const effectiveKind = resolveEffectiveKind(tool, kind, profile);
  if (effectiveKind === "search") return <SearchToolCard tool={tool} result={result} provenance={provenance} />;
  const Card = Object.hasOwn(KIND_CARDS, effectiveKind) ? KIND_CARDS[effectiveKind]! : GenericToolCard;
  return <Card tool={tool} result={result} />;
}

const KNOWN_KINDS: ReadonlySet<string> = new Set(["execute", "read", "edit", "delete", "search", "fetch", "think"]);

/** Trust a concrete ACP kind; otherwise map the tool name through the profile's
 *  aliases (codex `shell`, gemini `run_shell_command`, ...). */
function resolveEffectiveKind(tool: ToolCall, reclassifiedKind: string, profile: AgentProfile): string {
  if (KNOWN_KINDS.has(reclassifiedKind)) return reclassifiedKind;
  const name = tool.name?.trim() ?? "";
  if (!name) return reclassifiedKind;
  for (const [card, aliases] of Object.entries(profile.aliases) as [CardKind, string[]][]) {
    if (aliases.includes(name)) return card;
  }
  return reclassifiedKind;
}

function SubagentChildWrap({ children }: { children: ReactNode }) {
  return (
    <div className="border-l-2 border-accent-600/60 pl-2 ml-1">
      <div className="mb-0.5 inline-flex items-center gap-1 text-[10px] uppercase tracking-wider text-accent-600">
        <span>↳</span>
        <span>subagent</span>
      </div>
      {children}
    </div>
  );
}
