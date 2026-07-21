/**
 * On-disk conversation transcript for aoe-agent.
 *
 * aoe passes AOE_ARTIFACT_DIR (a per-session, restart-persistent app-data
 * dir). We append one completed user/assistant exchange per turn so that a
 * later `session/load` after an `aoe serve` restart can seed the model's
 * context. Only text turns are stored; that mirrors what the in-memory
 * history already holds (tool calls live inside a single streamText turn and
 * are never carried across turns), and keeps the format provider-neutral so a
 * transcript round-trips across a model switch.
 *
 * ponytail: append-only, one exchange per turn. A whole-file rewrite would be
 * O(n) per turn and could truncate the only copy on a mid-write crash. Switch
 * to a retention cap here if transcripts ever grow unbounded (#1005 open
 * question); today the model's context window bounds effective growth.
 */

import { appendFile, mkdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import type { ModelMessage } from "ai";

const TRANSCRIPT_FILE = "transcript.jsonl";

interface TurnMessage {
  role: "user" | "assistant";
  content: string;
}

/**
 * Append one completed exchange as a single write, so a crash between the two
 * records cannot leave a half-written pair (the write lands whole or a torn
 * tail that JSON.parse rejects on load). Creates the directory if aoe has not.
 */
export async function appendTurn(
  dir: string,
  user: string,
  assistant: string,
): Promise<void> {
  await mkdir(dir, { recursive: true });
  const line =
    JSON.stringify({ role: "user", content: user } satisfies TurnMessage) +
    "\n" +
    JSON.stringify(
      { role: "assistant", content: assistant } satisfies TurnMessage,
    ) +
    "\n";
  await appendFile(join(dir, TRANSCRIPT_FILE), line, { mode: 0o600 });
}

/**
 * Read the transcript back as ModelMessages. Malformed and schema-invalid
 * records are skipped rather than aborting the load. A trailing lone user
 * record (a turn whose assistant reply never got written, e.g. a crash between
 * the paired writes) is dropped so the resumed history always ends on an
 * assistant turn and stays strictly alternating. A missing file loads as an
 * empty history.
 */
export async function loadTranscript(dir: string): Promise<ModelMessage[]> {
  let raw: string;
  try {
    raw = await readFile(join(dir, TRANSCRIPT_FILE), "utf8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw err;
  }

  const messages: ModelMessage[] = [];
  for (const line of raw.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(trimmed);
    } catch {
      continue;
    }
    if (isTurnMessage(parsed)) messages.push(parsed);
  }

  if (messages.length > 0 && messages[messages.length - 1].role === "user") {
    messages.pop();
  }
  return messages;
}

function isTurnMessage(value: unknown): value is TurnMessage {
  if (typeof value !== "object" || value === null) return false;
  const role = (value as { role?: unknown }).role;
  const content = (value as { content?: unknown }).content;
  return (role === "user" || role === "assistant") && typeof content === "string";
}
