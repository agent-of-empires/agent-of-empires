// FAKE_ACP_SCRIPT for the structured view streaming benchmark: a history turn, then a measured
// turn gated on the fake agent's release file.

import { HOLD, chunk, endTurn, script } from "../helpers/acp";

export const HISTORY_DONE = "PERF_HISTORY_DONE";
export const MEASURED_DONE = "PERF_MEASURED_DONE";
export const MEASURED_CHUNKS = 500;
export const HISTORY_FENCE_SEED = "history";
export const MEASURED_FENCE_SEED = "measured";

type Update = object;

// The issue asks for 2,000 lines, which freezes the production bundle for 10+ minutes on load.
const DIFF_LINES = Number(process.env.PERF_DIFF_LINES) || 200;

const lines = (prefix: string, n: number) => Array.from({ length: n }, (_, i) => `${prefix} ${i}`).join("\n");
// Own lines, blank-separated, so Markdown sees a block; `seed` makes bodies differ for cold highlighting.
const fence = (seed: string, n: number) => `\n\n\`\`\`ts\n${lines(`const ${seed} =`, n)}\n\`\`\`\n\n`;

let nextId = 0;
const toolCall = (title: string, kind: string, extra: Record<string, unknown> = {}): [string, Update] => {
  const toolCallId = `perf-tc-${nextId++}`;
  return [toolCallId, { sessionUpdate: "tool_call", toolCallId, title, kind, status: "completed", ...extra }];
};

const editCall = (path: string, oldText: string, newText: string): Update =>
  toolCall(`Edit ${path}`, "edit", {
    rawInput: { file_path: path, old_string: oldText, new_string: newText },
    content: [{ type: "diff", path, oldText, newText }],
  })[1];

/** A Task parent plus `children` child tool calls linked through `_meta.claudeCode.parentToolUseId`. */
function taskWithChildren(title: string, children: number): Update[] {
  const [parentId, parent] = toolCall(`Task: ${title}`, "task");
  const kids = Array.from(
    { length: children },
    (_, i) =>
      toolCall(`Read file-${i}.ts`, "read", {
        rawInput: { file_path: `src/file-${i}.ts` },
        _meta: { claudeCode: { parentToolUseId: parentId } },
      })[1],
  );
  return [parent, ...kids];
}

function historyUpdates(): Update[] {
  const bigOld = lines("old line", DIFF_LINES);
  const bigNew = lines("new line", DIFF_LINES);
  const tools: Update[] = [];
  // 400 top-level tool calls: 50 Edit (every 8th), 349 reads, 1 Task with 200 child calls.
  for (let i = 0; i < 399; i++) {
    tools.push(
      i % 8 === 0
        ? editCall(`src/big-${i}.ts`, bigOld, bigNew)
        : toolCall(`Read file-${i}.ts`, "read", { rawInput: { file_path: `src/file-${i}.ts` } })[1],
    );
  }
  tools.push(...taskWithChildren("investigate", 200));
  // Spread tool calls and the 10 code-block chunks evenly among the 1,500 text chunks.
  const out: Update[] = [];
  const toolEvery = 1500 / tools.length;
  const codeEvery = 150;
  let t = 0;
  for (let i = 0; i < 1500; i++) {
    out.push(chunk(i % codeEvery === 0 ? fence(HISTORY_FENCE_SEED, 300) : `History line ${i}. `));
    while (t < tools.length && t <= (i + 1) / toolEvery) out.push(tools[t++]!);
  }
  while (t < tools.length) out.push(tools[t++]!);
  out.push(chunk(`\n\n${HISTORY_DONE}`));
  return out;
}

function measuredUpdates(): Update[] {
  const out: Update[] = [HOLD];
  const edits = 10;
  const bigOld = lines("old line", DIFF_LINES);
  const bigNew = lines("new line", DIFF_LINES);
  const task = taskWithChildren("measured", 20);
  for (let i = 0; i < MEASURED_CHUNKS; i++) {
    out.push(chunk(i === 100 ? fence(MEASURED_FENCE_SEED, 300) : `Streamed ${i} `), {
      sessionUpdate: "wait_ms",
      ms: 20,
    });
    if (i % 50 === 25 && i / 50 < edits) out.push(editCall(`src/live-${i}.ts`, bigOld, bigNew));
    if (i >= 200 && i < 200 + task.length) out.push(task[i - 200]!);
  }
  out.push(chunk(`\n\n${MEASURED_DONE}`));
  return out;
}

export function structuredScript(): object {
  return script(endTurn(...historyUpdates()), endTurn(...measuredUpdates()));
}
