#!/usr/bin/env node
/**
 * aoe-agent: ACP server wrapping Vercel AI SDK 6.
 *
 * One Node process per structured-view session. Accepts ACP requests from aoe
 * (the Rust ACP client) on stdin/stdout, drives a Vercel AI SDK loop
 * against the user's chosen provider, and streams structured events
 * back as ACP `session/update` notifications.
 *
 * Tools are stubs that delegate back to aoe via ACP `fs/*` and
 * `terminal/*` requests. aoe owns the disk; aoe-agent only orchestrates
 * the model.
 *
 * Lifecycle: stdin closes -> exit 0. SIGTERM -> graceful shutdown.
 */

import * as acp from "@agentclientprotocol/sdk";
import { Readable, Writable } from "node:stream";
import { streamText, tool, stepCountIs, type ModelMessage } from "ai";
import { anthropic } from "@ai-sdk/anthropic";
import { openai } from "@ai-sdk/openai";
import { google } from "@ai-sdk/google";
import { z } from "zod";
import { appendTurn, loadTranscript } from "./transcript.ts";

const DEFAULT_MODEL = "claude-opus-4-7";

interface SessionState {
  pendingPrompt: AbortController | null;
  modelId: string;
  /** Conversation history accumulated across turns within this session. */
  messages: ModelMessage[];
}

// ponytail: one Node process serves exactly one ACP connection, so a
// module-level session map is equivalent to the old per-connection instance
// state; no need to thread state through the connect handler.
const sessions = new Map<string, SessionState>();

async function handlePrompt(
  params: acp.PromptRequest,
  client: acp.AgentContext,
): Promise<acp.PromptResponse> {
  const session = sessions.get(params.sessionId);
  if (!session) {
    throw new Error(`Session ${params.sessionId} not found`);
  }

  session.pendingPrompt?.abort();
  session.pendingPrompt = new AbortController();
  const abortSignal = session.pendingPrompt.signal;

  const userText = params.prompt
    .filter((c): c is acp.TextContent & { type: "text" } => c.type === "text")
    .map((c) => c.text)
    .join("\n");

  session.messages.push({ role: "user", content: userText });

  const tools = buildTools(params.sessionId, client);

  try {
    const model = pickModel(session.modelId);
    const result = streamText({
      model,
      messages: session.messages,
      tools,
      // Allow up to ~16 tool-call rounds in a single user turn so the
      // agent can compose multiple Read/Write/Bash steps before
      // returning to the user.
      stopWhen: stepCountIs(16),
      abortSignal,
    });

    let assistantBuffer = "";
    const toolCallTitles = new Map<string, string>();
    for await (const part of result.fullStream) {
      if (abortSignal.aborted) break;
      switch (part.type) {
        case "text-delta": {
          const delta =
            (part as { text?: string }).text ??
            (part as { textDelta?: string }).textDelta ??
            "";
          if (!delta) break;
          assistantBuffer += delta;
          await client.notify("session/update", {
            sessionId: params.sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: delta },
            },
          });
          break;
        }
        case "tool-call": {
          const id = part.toolCallId;
          const name = part.toolName;
          toolCallTitles.set(id, name);
          await client.notify("session/update", {
            sessionId: params.sessionId,
            update: {
              sessionUpdate: "tool_call",
              toolCallId: id,
              title: name,
              kind: classifyKind(name),
              status: "pending",
              rawInput: part.input as Record<string, unknown>,
            },
          });
          break;
        }
        case "tool-result": {
          const id = part.toolCallId;
          await client.notify("session/update", {
            sessionId: params.sessionId,
            update: {
              sessionUpdate: "tool_call_update",
              toolCallId: id,
              status: "completed",
              rawOutput: serialiseToolOutput(part.output),
            },
          });
          break;
        }
        case "tool-error": {
          const id = part.toolCallId;
          await client.notify("session/update", {
            sessionId: params.sessionId,
            update: {
              sessionUpdate: "tool_call_update",
              toolCallId: id,
              status: "failed",
              rawOutput: { error: String(part.error) },
            },
          });
          break;
        }
        case "error": {
          const err = (part as { error: unknown }).error;
          throw err instanceof Error ? err : new Error(String(err));
        }
        default:
          break;
      }
    }

    // Anthropic rejects an assistant message with empty content, so skip
    // persisting blank turns (tool-only rounds and cancellations both leave
    // the buffer empty) to keep the next prompt's history valid.
    if (assistantBuffer) {
      session.messages.push({ role: "assistant", content: assistantBuffer });
    }

    if (abortSignal.aborted) {
      return { stopReason: "cancelled" };
    }

    // Persist only completed text exchanges, so the on-disk transcript stays
    // strictly alternating and never carries a dangling user turn across a
    // restart. Cancelled/errored and tool-only turns are skipped. Non-fatal:
    // a failed write must not fail the turn.
    const artifactDir = process.env.AOE_ARTIFACT_DIR;
    if (artifactDir && assistantBuffer) {
      try {
        await appendTurn(artifactDir, userText, assistantBuffer);
      } catch (err) {
        process.stderr.write(`[aoe-agent] transcript persist failed: ${err}\n`);
      }
    }

    session.pendingPrompt = null;
    return { stopReason: "end_turn" };
  } catch (err) {
    session.pendingPrompt = null;
    if (abortSignal.aborted) {
      return { stopReason: "cancelled" };
    }
    const message = err instanceof Error ? err.message : String(err);
    await client
      .notify("session/update", {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          content: {
            type: "text",
            text: `\n[aoe-agent error] ${message}\n`,
          },
        },
      })
      .catch(() => undefined);
    throw err;
  }
}

/**
 * Tool palette: Read, Write, Bash. Each tool's execute() body issues
 * an ACP request back to aoe and returns the result. The model never
 * sees the file system or shell directly.
 */
function buildTools(sessionId: string, client: acp.AgentContext) {
  return {
    Read: tool({
      description: "Read a text file from the session's working directory.",
      inputSchema: z.object({
        path: z.string().describe("Absolute path to the file to read."),
      }),
      execute: async ({ path }) => {
        const result = await client.request("fs/read_text_file", {
          sessionId,
          path,
        });
        return { content: result.content };
      },
    }),
    Write: tool({
      description:
        "Write text contents to a file in the session's working directory.",
      inputSchema: z.object({
        path: z.string().describe("Absolute path of the file to write."),
        content: z.string().describe("Full text content to write."),
      }),
      execute: async ({ path, content }) => {
        await client.request("fs/write_text_file", {
          sessionId,
          path,
          content,
        });
        return { ok: true };
      },
    }),
    Bash: tool({
      description:
        "Run a shell command and capture its output. Used for one-shot tasks; long-running processes are not supported.",
      inputSchema: z.object({
        command: z.string().describe("Shell command to run."),
        args: z
          .array(z.string())
          .optional()
          .describe("Arguments passed to the command."),
      }),
      execute: async ({ command, args }) => {
        const { terminalId } = await client.request("terminal/create", {
          sessionId,
          command,
          args: args ?? [],
        });
        try {
          const exit = await client.request("terminal/wait_for_exit", {
            sessionId,
            terminalId,
          });
          const out = await client.request("terminal/output", {
            sessionId,
            terminalId,
          });
          return {
            stdout: out.output,
            exitCode: exit.exitCode ?? null,
          };
        } finally {
          await client
            .request("terminal/release", { sessionId, terminalId })
            .catch(() => undefined);
        }
      },
    }),
  };
}

function classifyKind(toolName: string): acp.ToolKind {
  switch (toolName) {
    case "Read":
      return "read";
    case "Write":
      return "edit";
    case "Bash":
      return "execute";
    default:
      return "other";
  }
}

function serialiseToolOutput(output: unknown): Record<string, unknown> {
  if (output && typeof output === "object" && !Array.isArray(output)) {
    return output as Record<string, unknown>;
  }
  return { value: output };
}

function pickModel(modelId: string) {
  if (modelId.startsWith("claude-") || modelId.startsWith("anthropic:")) {
    return anthropic(modelId.replace(/^anthropic:/, ""));
  }
  if (modelId.startsWith("gpt-") || modelId.startsWith("openai:")) {
    return openai(modelId.replace(/^openai:/, ""));
  }
  if (modelId.startsWith("gemini-") || modelId.startsWith("google:")) {
    return google(modelId.replace(/^google:/, ""));
  }
  return anthropic(modelId);
}

function randomHexId(): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

function main() {
  const input = Writable.toWeb(process.stdout);
  const output = Readable.toWeb(process.stdin) as ReadableStream<Uint8Array>;
  const stream = acp.ndJsonStream(input, output);

  acp
    .agent({ name: "aoe-agent" })
    .onRequest("initialize", ({ params }) => ({
      protocolVersion: params.protocolVersion ?? acp.PROTOCOL_VERSION,
      agentCapabilities: {
        loadSession: true,
        promptCapabilities: {
          image: false,
          audio: false,
        },
      },
    }))
    .onRequest("authenticate", () => ({}))
    .onRequest("session/new", () => {
      const sessionId = randomHexId();
      const modelId = process.env.AOE_AGENT_MODEL ?? DEFAULT_MODEL;
      sessions.set(sessionId, {
        pendingPrompt: null,
        modelId,
        messages: [],
      });
      return { sessionId };
    })
    .onRequest("session/load", async ({ params }) => {
      // Reattach across an `aoe serve` restart: seed the model's context from
      // the persisted transcript. aoe rebuilds the UI from its own event
      // store and drops any transcript we might replay, so no session/update
      // replay is needed here, only restoring in-memory history. Registering
      // the session in the map is required, else the next session/prompt fails
      // with "Session not found".
      const artifactDir = process.env.AOE_ARTIFACT_DIR;
      let messages: ModelMessage[] = [];
      if (artifactDir) {
        try {
          messages = await loadTranscript(artifactDir);
        } catch (err) {
          process.stderr.write(
            `[aoe-agent] transcript load failed: ${err}\n`,
          );
        }
      }
      sessions.set(params.sessionId, {
        pendingPrompt: null,
        modelId: process.env.AOE_AGENT_MODEL ?? DEFAULT_MODEL,
        messages,
      });
      return {};
    })
    .onRequest("session/set_mode", () => ({}))
    .onRequest("session/prompt", ({ params, client }) =>
      handlePrompt(params, client),
    )
    .onNotification("session/cancel", ({ params }) => {
      sessions.get(params.sessionId)?.pendingPrompt?.abort();
    })
    .connect(stream);

  process.stdin.on("end", () => process.exit(0));
  process.on("SIGTERM", () => process.exit(0));
  process.on("SIGINT", () => process.exit(0));
}

main();
