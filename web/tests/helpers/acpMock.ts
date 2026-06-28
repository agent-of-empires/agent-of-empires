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

export interface AcpSessionMockOptions {
  sessionId?: string;
  title?: string;
  /** Events replayed onto the structured view WS as soon as it connects. */
  initialEvents?: unknown[];
  /** Maps a captured `POST .../acp/prompt` body to events replayed on
   *  the WS after the POST is fulfilled, standing in for the live
   *  fake-ACP agent's scripted turn. */
  onPrompt?: (body: { text: string }) => unknown[];
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
  promptBodies: Array<{ text: string }>;
  /** Parsed bodies of every `POST .../acp/config-option`. */
  configOptionBodies: Array<{ config_id: string; value: string }>;
  /** Parsed bodies of every `POST /api/telemetry/seen`. */
  telemetryPings: Array<{ surface?: string }>;
  /** Wrap events into frames and deliver them over the structured view
   *  WS (buffered until it connects). */
  pushEvents: (events: unknown[]) => void;
}

export async function mockAcpSession(page: Page, opts: AcpSessionMockOptions = {}): Promise<AcpSessionMock> {
  const sessionId = opts.sessionId ?? "sess-1";
  const title = opts.title ?? "acp-mock";

  let seq = 0;
  let ws: WebSocketRoute | null = null;
  const frameLog: string[] = [];
  const pushEvents = (events: unknown[]) => {
    for (const event of events) {
      const frame = JSON.stringify({ session_id: sessionId, seq: ++seq, event });
      frameLog.push(frame);
      ws?.send(frame);
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
  await page.route(/\/acp\/replay(\?|$)/, (r) => {
    const url = new URL(r.request().url());
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
          frames: page,
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
        frames: page,
        lost: false,
        highest_seq: highestSeq,
        lowest_seq: lowestSeq,
        next_cursor: page.length > 0 ? page[page.length - 1]!.seq : null,
        has_more: newer.length > limit,
      },
    });
  });
  await page.route("**/api/sessions/*/acp/prompt", async (r) => {
    const body = JSON.parse(r.request().postData() ?? "{}") as { text: string };
    handle.promptBodies.push(body);
    await r.fulfill({ json: {} });
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
