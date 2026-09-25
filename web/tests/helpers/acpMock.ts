// Mocked structured view session: REST stubs plus a WebSocket replaying `{ session_id, seq, event }` frames.
// The frame log is resent on reconnect like the daemon's drain; the reducer dedupes by seq.

import { expect, type Page, type WebSocketRoute } from "@playwright/test";

/** `prompt_id` is the client-minted id echoed on `UserPromptSent`. */
export interface AcpPromptBody {
  text: string;
  prompt_id?: string;
}

export interface AcpSessionMockOptions {
  sessionId?: string;
  title?: string;
  initialEvents?: unknown[];
  /** Events replayed after a prompt POST, standing in for the agent's turn. */
  onPrompt?: (body: AcpPromptBody) => unknown[];
  onConfigOption?: (body: { config_id: string; value: string }) => unknown[];
  about?: Record<string, unknown>;
  /** Report the session trashed with a stopped worker (#2529). */
  trashedAt?: string;
}

export interface AcpSessionMock {
  sessionId: string;
  title: string;
  promptBodies: AcpPromptBody[];
  configOptionBodies: Array<{ config_id: string; value: string }>;
  telemetryPings: Array<{ surface?: string }>;
  /** Deliver events as frames, buffered until the WS connects. */
  pushEvents: (events: unknown[]) => void;
}

/**
 * Minimal stand-in for the daemon's TranscriptModel (src/acp/transcript.rs) so mocked specs have rows to render.
 * Ids mirror the server's so merge-by-id behaves the same; fold correctness is tested in Rust and live specs.
 */
function foldEventToRow(event: unknown, seq: number, nextGroup: () => string, openMessageGroup: { id: string | null }) {
  const now = new Date().toISOString();
  // Unit variants serialize as a bare string.
  if (event === "SessionCleared") {
    openMessageGroup.id = null;
    return {
      id: `cleared-${seq}`,
      group_id: nextGroup(),
      kind: "session_cleared",
      at: now,
      text: "Conversation cleared, the model no longer remembers earlier turns.",
    };
  }
  if (typeof event !== "object" || event === null) return null;
  const ev = event as Record<string, Record<string, unknown>>;
  if (ev.AgentMessageChunk) {
    // Consecutive chunks share one group; any other event closes the run.
    const group = openMessageGroup.id ?? nextGroup();
    openMessageGroup.id = group;
    return {
      id: `msg-${seq}`,
      group_id: group,
      kind: "message",
      at: now,
      text: String(ev.AgentMessageChunk.text ?? ""),
    };
  }
  openMessageGroup.id = null;
  if (ev.UserPromptSent) {
    const promptId = ev.UserPromptSent.prompt_id;
    return {
      id: typeof promptId === "string" && promptId ? promptId : `user-seq-${seq}`,
      group_id: nextGroup(),
      kind: "user_prompt",
      at: now,
      text: String(ev.UserPromptSent.text ?? ""),
      attachments: ev.UserPromptSent.attachments ?? [],
    };
  }
  if (ev.ToolCallStarted) {
    const tc = ev.ToolCallStarted.tool_call as Record<string, unknown>;
    return {
      id: `start-${String(tc.id)}`,
      group_id: `tool-${String(tc.id)}`,
      kind: "tool_start",
      at: String(tc.started_at ?? now),
      text: String(tc.name ?? ""),
      tool_call_id: String(tc.id),
      tool: tc,
    };
  }
  if (ev.ToolCallCompleted) {
    const c = ev.ToolCallCompleted;
    const isError = Boolean(c.is_error);
    const content = String(c.content ?? "");
    return {
      id: `done-${String(c.tool_call_id)}`,
      group_id: `tool-${String(c.tool_call_id)}`,
      kind: isError ? "tool_error" : "tool_complete",
      at: String(c.completed_at ?? now),
      text: content || (isError ? "tool failed" : "completed"),
      tool_call_id: String(c.tool_call_id),
      output: c.output ?? [],
    };
  }
  return null;
}

export async function mockAcpSession(page: Page, opts: AcpSessionMockOptions = {}): Promise<AcpSessionMock> {
  const sessionId = opts.sessionId ?? "sess-1";
  const title = opts.title ?? "acp-mock";

  let seq = 0;
  let ws: WebSocketRoute | null = null;
  const frameLog: string[] = [];
  // Rows keep their seq so replay pages rows over the same window as frames.
  const rowLog: Array<{ seq: number; row: Record<string, unknown> }> = [];
  let groupCounter = 0;
  const nextGroup = () => `g${++groupCounter}`;
  const openMessageGroup: { id: string | null } = { id: null };
  const pushEvents = (events: unknown[]) => {
    for (const event of events) {
      const at = ++seq;
      const frame = JSON.stringify({ session_id: sessionId, seq: at, event });
      frameLog.push(frame);
      ws?.send(frame);
      const row = foldEventToRow(event, at, nextGroup, openMessageGroup);
      if (row) {
        rowLog.push({ seq: at, row });
        ws?.send(JSON.stringify({ kind: "transcript_delta", delta: { Append: row } }));
      }
    }
  };

  const handle: AcpSessionMock = {
    sessionId,
    title,
    promptBodies: [],
    configOptionBodies: [],
    telemetryPings: [],
    pushEvents,
  };

  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "system/update-status",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "docker/status" || path === "settings" || path === "system/update-status" ? {} : [],
      }),
    );
  }
  await page.route("**/api/about", (r) => r.fulfill({ json: opts.about ?? {} }));
  await page.route("**/api/telemetry/seen", (r) => {
    const body = r.request().postData();
    if (body) {
      try {
        handle.telemetryPings.push(JSON.parse(body));
      } catch {
        // Only well-formed pings are recorded.
      }
    }
    return r.fulfill({ status: 204 });
  });
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: sessionId,
            title,
            project_path: `/tmp/${title}`,
            group_path: "/tmp",
            tool: "claude",
            status: opts.trashedAt ? "Stopped" : "Running",
            yolo_mode: false,
            created_at: new Date().toISOString(),
            last_accessed_at: null,
            last_error: null,
            branch: null,
            main_repo_path: null,
            is_sandboxed: false,
            has_terminal: true,
            profile: "default",
            trashed_at: opts.trashedAt ?? null,
            workspace_repos: [],
            view: "structured",
            acp_worker_state: opts.trashedAt ? "stopped" : "running",
            claude_fullscreen: false,
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  // Registered first: later, more specific routes win.
  await page.route("**/api/sessions/*/acp/**", (r) => r.fulfill({ json: {} }));
  // Replay pages recent-first like the daemon so cold open and scroll-up paging run for real (#2236).
  // `?view=rows` returns rows with empty frames, as the daemon does.
  const isBoundary = (event: unknown): boolean =>
    typeof event === "object" && event !== null && ("UserPromptSent" in event || "UserDiffCommentsPrompt" in event);
  // Rows covering the same seq window as a frame page. `?view=rows` returns
  // the folded rows with an EMPTY `frames`, exactly like the daemon, so the
  // client's two-projection fetch gets its cursors from the frames leg.
  const rowsForWindow = (page: Array<{ seq: number }>) => {
    if (page.length === 0) return [];
    const lo = page[0]!.seq;
    const hi = page[page.length - 1]!.seq;
    return rowLog.filter((e) => e.seq >= lo && e.seq <= hi).map((e) => e.row);
  };
  await page.route(/\/acp\/replay(\?|$)/, (r) => {
    const url = new URL(r.request().url());
    const wantsRows = url.searchParams.get("view") === "rows";
    const limit = Number(url.searchParams.get("limit") ?? "1000");
    const frames = frameLog.map((f) => JSON.parse(f) as { seq: number; event: unknown });
    const highestSeq = frames.length > 0 ? frames[frames.length - 1]!.seq : 0;
    const lowestSeq = frames.length > 0 ? frames[0]!.seq : null;
    const beforeParam = url.searchParams.get("before");
    if (beforeParam != null) {
      const before = Number(beforeParam);
      const below = frames.filter((f) => f.seq < before);
      const hasMore = below.length > limit;
      let page = below.slice(Math.max(0, below.length - limit));
      if (hasMore) {
        const i = page.findIndex((f) => isBoundary(f.event));
        if (i > 0) page = page.slice(i);
      }
      return r.fulfill({
        json: {
          frames: wantsRows ? [] : page,
          ...(wantsRows ? { rows: rowsForWindow(page) } : {}),
          lost: false,
          highest_seq: highestSeq,
          lowest_seq: lowestSeq,
          next_cursor: page.length > 0 ? page[0]!.seq : null,
          has_more: hasMore,
        },
      });
    }
    const since = Number(url.searchParams.get("since") ?? "0");
    const newer = frames.filter((f) => f.seq > since);
    const page = newer.slice(0, limit);
    return r.fulfill({
      json: {
        frames: wantsRows ? [] : page,
        ...(wantsRows ? { rows: rowsForWindow(page) } : {}),
        lost: false,
        highest_seq: highestSeq,
        lowest_seq: lowestSeq,
        next_cursor: page.length > 0 ? page[page.length - 1]!.seq : null,
        has_more: newer.length > limit,
      },
    });
  });
  await page.route("**/api/sessions/*/acp/prompt", async (r) => {
    const body = JSON.parse(r.request().postData() ?? "{}") as AcpPromptBody;
    handle.promptBodies.push(body);
    await r.fulfill({ json: {} });
    // The daemon echoes `UserPromptSent` before the agent replies; without it the composer stays working (#3417).
    pushEvents([{ UserPromptSent: { text: body.text, prompt_id: body.prompt_id ?? null } }]);
    pushEvents(opts.onPrompt?.(body) ?? []);
  });
  await page.route("**/api/sessions/*/acp/config-option", async (r) => {
    const body = JSON.parse(r.request().postData() ?? "{}") as {
      config_id: string;
      value: string;
    };
    handle.configOptionBodies.push(body);
    await r.fulfill({ json: {} });
    pushEvents(opts.onConfigOption?.(body) ?? []);
  });

  await page.routeWebSocket(/\/sessions\/[^/]+\/ws(\?|$)/, () => {
    // no-op
  });
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, (route) => {
    ws = route;
    for (const frame of frameLog) route.send(frame);
    route.send(JSON.stringify({ kind: "transcript_snapshot", rows: rowLog.map((e) => e.row) }));
  });

  pushEvents(opts.initialEvents ?? []);
  return handle;
}

/** Deep link rather than a sidebar click, which fails at mobile widths. */
export async function openStructuredSession(page: Page, mock: AcpSessionMock) {
  await page.goto(`/session/${mock.sessionId}`);
  await expect(page.locator("header")).toBeVisible();
}

/** The Send button reads "Send message" only once the WS is open; earlier sends queue instead. */
export async function waitForComposerConnected(page: Page) {
  await expect(page.getByRole("button", { name: "Send message" })).toBeVisible({ timeout: 10_000 });
}

/* ── AcpEvent builders (externally-tagged serde shapes) ──────────── */

export function agentMessageChunk(text: string) {
  return { AgentMessageChunk: { text } };
}

export function stopped(reason = "end_turn") {
  return { Stopped: { reason } };
}

export function toolCallStarted(tc: { id: string; name: string; kind: string; args_preview: string }) {
  return {
    ToolCallStarted: {
      tool_call: { ...tc, started_at: new Date().toISOString() },
    },
  };
}

export function backgroundAgentLaunched(fields: {
  agent_id: string;
  tool_call_id: string;
  description: string;
  prompt: string;
  model: string;
}) {
  return {
    BackgroundAgentLaunched: { ...fields, started_at: new Date().toISOString() },
  };
}

export function configOptionsUpdated(options: unknown[]) {
  return { ConfigOptionsUpdated: { options } };
}

export function usageUpdated(usage: { used: number; size: number; cost: { amount: number; currency: string } | null }) {
  return { UsageUpdated: { usage } };
}
