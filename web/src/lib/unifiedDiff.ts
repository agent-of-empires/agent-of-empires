import type { RichDiffHunk } from "./types";

const PREFIX = { add: "+", delete: "-", equal: " " } as const;

/** One hunk as a unified diff; a file with no old lines is a creation. */
export function unifiedDiff(path: string, hunk: RichDiffHunk): string {
  const range = `@@ -${hunk.old_start},${hunk.old_lines} +${hunk.new_start},${hunk.new_lines} @@`;
  return [
    `--- ${hunk.old_lines === 0 ? "/dev/null" : path}`,
    `+++ ${path}`,
    range,
    ...hunk.lines.map((line) => PREFIX[line.type] + line.content),
  ].join("\n");
}
