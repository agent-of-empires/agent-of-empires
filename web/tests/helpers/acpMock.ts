// Shared scaffolding for mocked structured-view (ACP) specs.
//
// Mirrors the route stack proven by acp-edit-card-diff.spec.ts: REST
// stubs for the app shell, a single structured-view session in the
// sidebar, a swallowed terminal WebSocket, and a scripted structured
// view WebSocket that replays the daemon's `AcpBroadcastFrame` wire
// shape (web/src/lib/acpTypes.ts). Specs push externally-tagged
// `AcpEvent` values; the helper wraps each in
// `{ session_id, seq, event }` with a monotonically increasing seq.
// The full frame log is re-sent to a late or reconnecting socket,
// mimicking the server's on-connect drain; the reducer's seq dedupe
// drops the duplicates.

import { expect, type Page, type WebSocketRoute } from "@playwright/test";

/** Parsed `POST .../acp/prompt` request body. `prompt_id` is the
 *  client-minted id the daemon echoes back on `UserPromptSent`, so a spec can
 *  assert the optimistic-row correlation directly. Optional because a caller
 *  that predates the id (or posts by hand) may omit it. */
export interface AcpPromptBody {
  text: string;
  prompt_id?: string;
}

export interface AcpSessionMockOptions {
  sessionId?: string;
  title?: string;
  /** Events replayed onto the structured view WS as soon as it connects. */
  initialEvents?: unknown[];
  /** Maps a captured `POST .../acp/prompt` body to events replayed on
   *  the WS after the POST is fulfilled, standing in for the live
   *  fake-ACP agent's scripted turn. */
  onPrompt?: (body: AcpPromptBody) => unknown[];
  /** Same, for `POST .../acp/config-option`: the returned events play
   *  the adapter's confirming snapshot (or rejection). */
  onConfigOption?: (body: { config_id: string; value: string }) => unknown[];
  /** Override the `/api/about` payload (e.g. `{ read_only: true }`). */
  about?: Record<string, unknown>;
  /** When set, the session is reported trashed (`trashed_at`) with a stopped
   *  worker, so the trashed read-only banner shows. See #2529. */
  trashedAt?: string;
}

export interface AcpSessionMock {
  sessionId: string;
  title: string;
  /** Parsed bodies of every `POST .../acp/prompt` the page sent. */
  promptBodies: AcpPromptBody[];
  /** Parsed bodies of every `POST .../acp/config-option`. */
  configOptionBodies: Array<{ config_id: string; value: string }>;
  /** Parsed bodies of every `POST /api/telemetry/seen`. */
  telemetryPings: Array<{ surface?: string }>;
  /** Wrap events into frames and deliver them over the structured view
   *  WS (buffered until it connects). */
  pushEvents: (events: unknown[]) => void;
}

/** Test double for the daemon's `TranscriptModel` (src/acp/transcript.rs),
 *  covering only the event kinds the mocked specs push: agent message chunks,
 *  user prompts, and the two tool-lifecycle edges. Everything else folds to no
 *  row, which is what the server does for the control-only events these specs
 *  use (`Stopped`, `ConfigOptionsUpdated`, ...).
 *
 *  Row ids and group ids mirror the server's (`msg-<seq>`, `user-seq-<seq>`,
 *  `start-<id>` / `done-<id>` grouped under `tool-<id>`, `g<n>` for the rest)
 *  so merge-by-id behaves as it does in production. Fold *correctness* is not
 *  what this suite tests: the Rust unit tests in `transcript.rs` and the live
 *  Playwright specs cover that against the real fold. This exists so mocked
 *  browser-behavior specs (fonts, scrolling, touch, card layout) have a
 *  transcript to render at all now that the client no longer folds events
 *  itself.
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
  // Server-folded transcript rows, tagged with the seq that produced them so
  // the replay route can page rows over the same seq window as the frames.
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
      // The daemon folds the event and ships the row separately (Tier 4); the
      // raw frame above now feeds the client's control reducer only.
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
        // Only well-formed `{ surface }` posts matter to the specs.
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
  // Structured view REST endpoints (snapshot/...): empty is fine,
  // everything interesting arrives over the WebSocket. Registered before
  // the prompt/config-option captures so those (later, more specific)
  // routes win Playwright's reverse-registration-order matching.
  await page.route("**/api/sessions/*/acp/**", (r) => r.fulfill({ json: {} }));
  // Replay endpoint: serve the frame log with the real recent-first
  // paging contract so the client's cold-open (tail via `before`) and
  // scroll-up (older pages via `before`) paths are exercised, not stubbed.
  // Registered after the generic acp/** route so it wins for replay URLs.
  // See #2236.
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
    // The daemon publishes `UserPromptSent` carrying the client-minted
    // `prompt_id` BEFORE it forwards the prompt to the agent
    // (`send_turn` -> `publish_user_prompt_with_attachments`). That echo is
    // what settles the client's optimistic in-flight marker and opens the
    // turn, so a mock that jumps straight to the agent's reply leaves the
    // composer wedged in its "working" state. See #3417.
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

  // Terminal WS (only opened outside structured view mode): swallow it.
  await page.routeWebSocket(/\/sessions\/[^/]+\/ws(\?|$)/, () => {
    // no-op
  });
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, (route) => {
    ws = route;
    for (const frame of frameLog) route.send(frame);
    // The daemon's on-connect transcript snapshot. Merged by row id, so
    // re-sending rows the client already has from replay is a no-op.
    route.send(JSON.stringify({ kind: "transcript_snapshot", rows: rowLog.map((e) => e.row) }));
  });

  pushEvents(opts.initialEvents ?? []);
  return handle;
}

/** Open the mocked structured-view session via its deep link. Direct
 *  navigation rather than a sidebar click: several consumers run at
 *  mobile widths where the sidebar is collapsed and the session link is
 *  outside the viewport (sidebar-row navigation has its own spec). */
export async function openStructuredSession(page: Page, mock: AcpSessionMock) {
  await page.goto(`/session/${mock.sessionId}`);
  await expect(page.locator("header")).toBeVisible();
}

/** Wait until the composer reflects an open structured view WS. The Send
 *  button only reads "Send message" while `status === "open"`; sending
 *  before that would queue the prompt instead of POSTing it. */
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

export function toolCallCompleted(fields: { tool_call_id: string; is_error: boolean; content: string }) {
  return {
    ToolCallCompleted: { ...fields, completed_at: new Date().toISOString() },
  };
}

export function configOptionsUpdated(options: unknown[]) {
  return { ConfigOptionsUpdated: { options } };
}

export function configOptionSwitchFailed(config_id: string, value: string, reason: string) {
  return { ConfigOptionSwitchFailed: { config_id, value, reason } };
}
