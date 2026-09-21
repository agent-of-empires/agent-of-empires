// Cards for the ACP ToolKind categories (execute, read, edit, delete, search, fetch, think) and the fallback.

import { useMemo } from "react";
import { FileText, Globe, Pencil, Search, Sparkles, Terminal, Trash2 } from "lucide-react";

import { parseJsonObject, pickFirst, pickStr } from "../../lib/acpArgs";
import type { ToolCall } from "../../lib/acpTypes";
import { diffPair } from "../../lib/diffPair";
import { relativeDisplayPath } from "../../lib/fileRef";
import { StringDiff } from "../diff/StringDiff";
import { useAcpFileRef } from "./AcpFileRefContext";
import {
  CardChrome,
  ClickableFilePath,
  HighlightedBlock,
  PlaceholderLine,
  RawBlock,
  statusFor,
  unwrapMarkdownFence,
  useToolCardExpansion,
  type ToolCardProps,
} from "./ToolCardChrome";
import { ToolErrorBody } from "./ToolErrorBody";

const ICON = "h-3.5 w-3.5";
const META = "hidden md:inline text-[11px] text-text-dim";

/** Fallback chain for a header's key field: real arg, then the ACP title, then the tool name. */
function primaryArg(tool: ToolCall, args: Record<string, unknown> | null, ...keys: string[]) {
  return pickFirst(pickStr(args, ...keys), pickStr(args, "_aoe_title"), tool.name);
}

function useFilePath(rawPath: string) {
  const { fileRefSession } = useAcpFileRef();
  return relativeDisplayPath(rawPath, fileRefSession);
}

const PATH_KEYS = ["path", "file_path", "filePath", "filename"];

export function ExecuteToolCard({ tool, result }: ToolCardProps) {
  const status = statusFor(result);
  const args = parseJsonObject(tool.args_preview);
  const command = primaryArg(tool, args, "command", "cmd", "args") ?? "(no command)";
  const description = pickStr(args, "description");
  const output = result?.text ?? "";
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Terminal className={ICON} />}
      label="bash"
      primary={
        <>
          <span className="mr-1 text-text-dim">$</span>
          {command}
        </>
      }
      meta={
        output && status !== "running" ? (
          <span className={META}>{unwrapMarkdownFence(output).text.split("\n").length} lines</span>
        ) : undefined
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {description && (
            <div className="border-t border-surface-800 bg-surface-900/40 px-3 py-1 text-[11px] text-text-muted italic">
              {description}
            </div>
          )}
          {/* The header truncates; the body shows the full command. */}
          <HighlightedBlock text={command} language="bash" maxLines={6} />
          {output && status !== "err" ? (
            <HighlightedBlock text={output} language="bash" maxLines={20} />
          ) : status !== "err" ? (
            <PlaceholderLine>{status === "running" ? "Running…" : "(no output)"}</PlaceholderLine>
          ) : null}
        </ToolErrorBody>
      }
    />
  );
}

function formatRange(args: Record<string, unknown> | null): string | null {
  if (!args) return null;
  const offset = typeof args.offset === "number" ? args.offset : null;
  const limit = typeof args.limit === "number" ? args.limit : null;
  if (offset !== null && limit !== null) return `L${offset}–${offset + limit}`;
  if (offset !== null) return `from L${offset}`;
  if (limit !== null) return `${limit} lines`;
  return null;
}

export function ReadToolCard({ tool, result }: ToolCardProps) {
  const status = statusFor(result);
  const args = parseJsonObject(tool.args_preview);
  const argPath = pickStr(args, ...PATH_KEYS);
  const rawPath = primaryArg(tool, args, ...PATH_KEYS) ?? "(unknown file)";
  const path = useFilePath(rawPath);
  const range = formatRange(args);
  const ext = argPath?.match(/\.([a-z0-9]+)$/i)?.[1]?.toLowerCase();
  const content = result?.text ?? "";
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<FileText className={ICON} />}
      label="read"
      primary={<ClickableFilePath rawPath={rawPath} display={path} />}
      meta={
        <>
          {range && <span className="text-[11px] text-text-dim">{range}</span>}
          {content && <span className={META}>{content.split("\n").length} lines</span>}
        </>
      }
      expanded={open}
      onToggle={status === "err" || content ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {content && status !== "err" && <HighlightedBlock text={content} language={ext} maxLines={16} />}
        </ToolErrorBody>
      }
    />
  );
}

export function EditToolCard({ tool, result }: ToolCardProps) {
  const status = statusFor(result);
  const { fileRefSession } = useAcpFileRef();
  const args = parseJsonObject(tool.args_preview);
  // Structured per-file diffs (ToolCallContent::Diff) win over old/new string args.
  const structuredDiffs = tool.diffs ?? [];
  const hasStructuredDiffs = structuredDiffs.length > 0;
  const legacyOld = pickStr(args, "old_string", "oldString", "old_str") ?? "";
  const legacyNew = pickStr(args, "new_string", "newString", "new_str", "content") ?? "";
  const hasLegacyDiff = legacyOld !== "" || legacyNew !== "";
  const rawPath =
    pickFirst(structuredDiffs[0]?.path, pickStr(args, ...PATH_KEYS), pickStr(args, "_aoe_title"), tool.name) ??
    "(unknown file)";
  const path = relativeDisplayPath(rawPath, fileRefSession);
  const [open, setOpen] = useToolCardExpansion(status);
  const hasDiff = hasStructuredDiffs || hasLegacyDiff;
  const isEdit = hasStructuredDiffs ? structuredDiffs.some((d) => (d.old_text ?? "") !== "") : legacyOld !== "";
  const multiFile = structuredDiffs.length > 1;

  const { adds, dels } = useMemo(() => {
    const ds = tool.diffs ?? [];
    if (ds.length === 0) return diffPair(legacyOld, legacyNew);
    return ds.reduce(
      (acc, d) => {
        const p = diffPair(d.old_text ?? "", d.new_text ?? "");
        return { adds: acc.adds + p.adds, dels: acc.dels + p.dels };
      },
      { adds: 0, dels: 0 },
    );
  }, [tool.diffs, legacyOld, legacyNew]);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Pencil className={ICON} />}
      label={isEdit ? "edit" : "write"}
      primary={
        <ClickableFilePath
          rawPath={rawPath}
          display={multiFile ? `${path} +${structuredDiffs.length - 1} more` : path}
        />
      }
      // No diff summary on failure: nothing landed.
      meta={
        status !== "err" &&
        hasDiff &&
        (adds > 0 || dels > 0) && (
          <span className="hidden md:inline text-[11px]">
            <span className="text-emerald-400">+{adds}</span> <span className="text-rose-400">−{dels}</span>
          </span>
        )
      }
      expanded={open}
      onToggle={status === "err" || hasDiff ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasStructuredDiffs ? (
            <div className="border-t border-surface-800 bg-surface-950">
              {structuredDiffs.map((d, i) => (
                <div key={`${d.path}-${i}`}>
                  {multiFile && (
                    <div className="px-2 py-1 text-[11px] text-text-dim" title={d.path}>
                      {relativeDisplayPath(d.path, fileRefSession)}
                    </div>
                  )}
                  <StringDiff oldText={d.old_text ?? ""} newText={d.new_text ?? ""} filePath={d.path} />
                </div>
              ))}
            </div>
          ) : (
            hasLegacyDiff && (
              <div className="border-t border-surface-800 bg-surface-950">
                <StringDiff oldText={legacyOld} newText={legacyNew} filePath={path} />
              </div>
            )
          )}
        </ToolErrorBody>
      }
    />
  );
}

export function DeleteToolCard({ tool, result }: ToolCardProps) {
  const status = statusFor(result);
  const rawPath = primaryArg(tool, parseJsonObject(tool.args_preview), ...PATH_KEYS) ?? "(unknown file)";
  const path = useFilePath(rawPath);
  const [open, setOpen] = useToolCardExpansion(status);
  const failed = status === "err";
  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Trash2 className={`${ICON} text-rose-400`} />}
      label="delete"
      primary={<span title={rawPath}>{path}</span>}
      expanded={open}
      onToggle={failed ? () => setOpen((v) => !v) : undefined}
      body={
        failed ? (
          <ToolErrorBody status={status} errorText={result?.text}>
            {null}
          </ToolErrorBody>
        ) : undefined
      }
    />
  );
}

/** `provenance: "bash"` marks a grep/find/rg shell-out rerouted to this card. */
export function SearchToolCard({ tool, result, provenance }: ToolCardProps & { provenance?: "bash" | null }) {
  const status = statusFor(result);
  const args = parseJsonObject(tool.args_preview);
  const query =
    pickFirst(
      pickStr(args, "query", "pattern", "q", "search"),
      pickStr(args, "_aoe_title"),
      pickStr(args, "command"),
      tool.name,
    ) ?? "(no query)";
  const path = pickStr(args, "path", "directory", "scope");
  const output = result?.text ?? "";
  const lines = output ? output.split("\n").filter(Boolean) : [];
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Search className={ICON} />}
      label={provenance === "bash" ? "search · bash" : "search"}
      primary={query}
      meta={
        <>
          {path && <span className={META}>in {path}</span>}
          {lines.length > 0 && (
            <span className="text-[11px] text-text-dim">
              {lines.length} match{lines.length === 1 ? "" : "es"}
            </span>
          )}
        </>
      }
      expanded={open}
      onToggle={status === "err" || lines.length > 0 ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {lines.length > 0 && status !== "err" && (
            <div className="border-t border-surface-800 bg-surface-950 max-h-64 overflow-y-auto">
              {lines.slice(0, 50).map((l, i) => (
                <div key={i} className="flex font-mono text-[11px] hover:bg-surface-900">
                  <span className="select-none w-10 shrink-0 px-2 py-0.5 text-right text-text-dim">{i + 1}</span>
                  <span className="px-2 py-0.5 text-text-secondary truncate">{l}</span>
                </div>
              ))}
              {lines.length > 50 && (
                <div className="border-t border-surface-800 px-3 py-1 text-center text-[11px] text-text-dim">
                  {lines.length - 50} more match
                  {lines.length - 50 === 1 ? "" : "es"}
                </div>
              )}
            </div>
          )}
        </ToolErrorBody>
      }
    />
  );
}

export function FetchToolCard({ tool, result }: ToolCardProps) {
  const status = statusFor(result);
  const url = primaryArg(tool, parseJsonObject(tool.args_preview), "url", "uri", "endpoint") ?? "(no url)";
  const output = result?.text ?? "";
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Globe className={ICON} />}
      label="fetch"
      primary={url}
      expanded={open}
      onToggle={status === "err" || output ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {output && status !== "err" && <HighlightedBlock text={output} language="json" maxLines={16} />}
        </ToolErrorBody>
      }
    />
  );
}

export function ThinkToolCard({ tool }: ToolCardProps) {
  return (
    <div className="my-1 flex items-center gap-2 px-3 py-1 text-xs italic text-text-muted">
      <Sparkles className="h-3 w-3 text-text-dim" />
      <span>{tool.name || "thinking…"}</span>
    </div>
  );
}

export function GenericToolCard({ tool, result }: ToolCardProps) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);
  const output = result?.text ?? "";
  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Sparkles className={ICON} />}
      label={tool.kind || "tool"}
      primary={tool.name}
      expanded={open}
      onToggle={status === "err" || tool.args_preview || output ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {tool.args_preview && <RawBlock label="input" text={tool.args_preview} />}
          {output && status !== "err" && <RawBlock label="output" text={output} />}
        </ToolErrorBody>
      }
    />
  );
}
