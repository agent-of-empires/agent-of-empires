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

interface SessionState {
  pendingPrompt: AbortController | null;
  modelId: string;
  /** Conversation history accumulated across turns within this session. */
  messages: ModelMessage[];
}

class AoeAgent implements acp.Agent {
  private connection: acp.AgentSideConnection;
  private sessions: Map<string, SessionState>;

  constructor(connection: acp.AgentSideConnection) {
    this.connection = connection;
    this.sessions = new Map();
  }

  async initialize(
    params: acp.InitializeRequest,
  ): Promise<acp.InitializeResponse> {
    return {
      protocolVersion: params.protocolVersion ?? acp.PROTOCOL_VERSION,
      agentCapabilities: {
        loadSession: false,
        promptCapabilities: {
          image: false,
          audio: false,
        },
      },
    };
  }

  async authenticate(
    _params: acp.AuthenticateRequest,
  ): Promise<acp.AuthenticateResponse | void> {
    return {};
  }

  async newSession(
    _params: acp.NewSessionRequest,
  ): Promise<acp.NewSessionResponse> {
    const sessionId = randomHexId();
    const modelId = process.env.AOE_AGENT_MODEL ?? "claude-opus-4-7";
    this.sessions.set(sessionId, {
      pendingPrompt: null,
      modelId,
      messages: [],
    });
    return { sessionId };
  }

  async setSessionMode(
    _params: acp.SetSessionModeRequest,
  ): Promise<acp.SetSessionModeResponse> {
    return {};
  }

  async prompt(params: acp.PromptRequest): Promise<acp.PromptResponse> {
    const session = this.sessions.get(params.sessionId);
    if (!session) {
      throw new Error(`Session ${params.sessionId} not found`);
    }

    session.pendingPrompt?.abort();
    session.pendingPrompt = new AbortController();
    const abortSignal = session.pendingPrompt.signal;

    const userText = params.prompt
      .filter((c): c is acp.TextContentBlock => c.type === "text")
      .map((c) => c.text)
      .join("\n");

    session.messages.push({ role: "user", content: userText });

    const tools = this.buildTools(params.sessionId);

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
            await this.connection.sessionUpdate({
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
            await this.connection.sessionUpdate({
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
            await this.connection.sessionUpdate({
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
            await this.connection.sessionUpdate({
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

      session.messages.push({ role: "assistant", content: assistantBuffer });

      if (abortSignal.aborted) {
        return { stopReason: "cancelled" };
      }
      session.pendingPrompt = null;
      return { stopReason: "end_turn" };
    } catch (err) {
      session.pendingPrompt = null;
      if (abortSignal.aborted) {
        return { stopReason: "cancelled" };
      }
      const message = err instanceof Error ? err.message : String(err);
      await this.connection
        .sessionUpdate({
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

  async cancel(params: acp.CancelNotification): Promise<void> {
    this.sessions.get(params.sessionId)?.pendingPrompt?.abort();
  }

  /**
   * Tool palette: Read, Write, Bash. Each tool's execute() body issues
   * an ACP request back to aoe and returns the result. The model never
   * sees the file system or shell directly.
   */
  private buildTools(sessionId: string) {
    return {
      Read: tool({
        description:
          "Read a text file from the session's working directory.",
        inputSchema: z.object({
          path: z.string().describe("Absolute path to the file to read."),
        }),
        execute: async ({ path }) => {
          const result = await this.connection.readTextFile({
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
          await this.connection.writeTextFile({
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
          const term = await this.connection.createTerminal({
            sessionId,
            command,
            args: args ?? [],
          });
          try {
            const exit = await term.waitForExit();
            const out = await term.currentOutput();
            const code =
              (exit as { exitCode?: number }).exitCode ??
              (exit as { exit_code?: number }).exit_code ??
              null;
            return {
              stdout: out.output,
              exitCode: code,
            };
          } finally {
            await term.release().catch(() => undefined);
          }
        },
      }),
    };
  }
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
  new acp.AgentSideConnection((conn) => new AoeAgent(conn), stream);

  process.stdin.on("end", () => process.exit(0));
  process.on("SIGTERM", () => process.exit(0));
  process.on("SIGINT", () => process.exit(0));
}

main();
