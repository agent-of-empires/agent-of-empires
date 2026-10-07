/** Native-ID-scoped text history within the AoE instance artifact directory. */
import { constants } from "node:fs";
import { mkdir, open, readFile } from "node:fs/promises";
import { join } from "node:path";
import type { ModelMessage } from "ai";

function transcriptPath(dir: string, sessionId: string): string {
  if (!/^[a-f0-9]{32}$/.test(sessionId)) {
    throw new Error("Invalid aoe-agent session ID");
  }
  return join(dir, `aoe-agent-${sessionId}.jsonl`);
}

/** Publish even an empty conversation before acknowledging session/new. */
export async function createTranscript(
  dir: string,
  sessionId: string,
): Promise<void> {
  const path = transcriptPath(dir, sessionId);
  await mkdir(dir, { recursive: true });
  const file = await open(path, "wx", 0o600);
  try {
    await file.sync();
  } finally {
    await file.close();
  }
  const directory = await open(dir, "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
}

interface TurnMessage {
  role: "user" | "assistant";
  content: string;
}

/** Append only to the conversation created for this native ID. */
export async function appendTurn(
  dir: string,
  sessionId: string,
  user: string,
  assistant: string,
): Promise<void> {
  const line =
    JSON.stringify({ role: "user", content: user } satisfies TurnMessage) +
    "\n" +
    JSON.stringify({
      role: "assistant",
      content: assistant,
    } satisfies TurnMessage) +
    "\n";
  const file = await open(
    transcriptPath(dir, sessionId),
    constants.O_WRONLY | constants.O_APPEND,
  );
  try {
    await file.writeFile(line);
  } finally {
    await file.close();
  }
}

/** Skip malformed records and a torn trailing user turn; missing IDs fail. */
export async function loadTranscript(
  dir: string,
  sessionId: string,
): Promise<ModelMessage[]> {
  const raw = await readFile(transcriptPath(dir, sessionId), "utf8");

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
  return (
    (role === "user" || role === "assistant") && typeof content === "string"
  );
}
