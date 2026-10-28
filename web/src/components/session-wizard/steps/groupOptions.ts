import type { GroupInfo } from "../../../lib/types";

export type GroupOption = { kind: "group"; group: GroupInfo } | { kind: "create"; path: string };

/** The list for the typed text: matching groups (archived last), then a create row
 *  when nothing matches exactly. `highlight` is the exact match or the create row. */
export function groupOptions(groups: GroupInfo[], text: string): { options: GroupOption[]; highlight: number } {
  const query = text.trim();
  const needle = query.toLowerCase();
  const matches = groups.filter((g) => g.path.toLowerCase().includes(needle));
  const ordered = [...matches.filter((g) => !g.archived), ...matches.filter((g) => g.archived)];
  const options: GroupOption[] = ordered.map((group) => ({ kind: "group", group }));
  const exact = options.findIndex((o) => o.kind === "group" && o.group.path === query);
  if (!query) return { options, highlight: -1 };
  if (exact >= 0) return { options, highlight: exact };
  options.push({ kind: "create", path: query });
  return { options, highlight: options.length - 1 };
}

export function optionLabel(option: GroupOption): string {
  if (option.kind === "create") return `Create group "${option.path}"`;
  const { path, session_count, archived } = option.group;
  const count = `${session_count} ${session_count === 1 ? "session" : "sessions"}`;
  return archived ? `${path} · ${count} · archived` : `${path} · ${count}`;
}
