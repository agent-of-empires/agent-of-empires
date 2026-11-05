import { expect, type Locator, type Page, type TestInfo } from "@playwright/test";
import { readFileSync, existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fakeAcpScriptPath, listSessions, seedSessionViaAoeAdd, type ServeHandle } from "./aoeServe";
import type { ServeOptions } from "./liveTest";

function debugLogPath(home: string): string | undefined {
  // Linux resolves the app dir under XDG_CONFIG_HOME; macOS/Windows under the legacy dot dir.
  return [
    join(home, "config", "agent-of-empires-dev", "debug.log"),
    join(home, ".agent-of-empires-dev", "debug.log"),
  ].find((p) => existsSync(p));
}

function tail(content: string, bytes: number): string {
  return content.length > bytes ? `... (${content.length - bytes} bytes elided)\n` + content.slice(-bytes) : content;
}

function debugLogTail(home: string | undefined): string {
  const path = home && debugLogPath(home);
  try {
    return path ? tail(readFileSync(path, "utf8"), 8_000) : "(debug.log unavailable)";
  } catch {
    return "(debug.log unavailable)";
  }
}

export async function replayFrames(baseUrl: string, sessionId: string): Promise<unknown[]> {
  const replay = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
  return Array.isArray(replay) ? replay : ((replay?.frames as unknown[]) ?? []);
}

/** The startup failure recorded by the ACP worker. */
function findStartupError(frames: unknown[]): string | null {
  for (const f of frames) {
    const event = (f as { event?: { AgentStartupError?: { message?: string } } } | null)?.event;
    if (event && typeof event === "object" && "AgentStartupError" in event) {
      return event.AgentStartupError?.message ?? "AgentStartupError (no message)";
    }
  }
  return null;
}

/** Wait for the live projection's current owned Available receipt, not worker presence or replayed readiness. */
export async function waitForAcpReady(
  baseUrl: string,
  sessionId: string,
  timeoutMs = 30_000,
  home?: string,
): Promise<void> {
  let lastReducedState: unknown = null;
  try {
    const { promise, resolve, reject } = Promise.withResolvers<void>();
    const url = new URL(`/sessions/${encodeURIComponent(sessionId)}/acp/ws?since=0`, baseUrl);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(url, ["aoe-auth"]);
    let lastSeq = -1;
    const finish = (error?: Error) => {
      clearTimeout(timer);
      socket.onmessage = null;
      socket.onerror = null;
      socket.onclose = null;
      socket.close();
      if (error) reject(error);
      else resolve();
    };
    const timer = setTimeout(
      () => finish(new Error(`No current owned ACP ready receipt within ${timeoutMs}ms`)),
      timeoutMs,
    );
    socket.onerror = () => finish(new Error("ACP readiness WebSocket failed"));
    socket.onclose = (event) =>
      finish(new Error(`ACP readiness WebSocket closed before ready: ${event.code} ${event.reason}`));
    socket.onmessage = (message) => {
      try {
        const frame = JSON.parse(String(message.data)) as {
          kind?: string;
          session_id?: string;
          seq?: number;
          state?: { connection_ready?: boolean; connection_unavailable?: unknown };
        };
        if (frame.session_id !== sessionId) return;
        const startupError = findStartupError([frame]);
        if (startupError) {
          finish(new Error(`ACP startup failed: ${startupError}`));
          return;
        }
        if (
          frame.kind !== "reduced_state" ||
          typeof frame.seq !== "number" ||
          !Number.isSafeInteger(frame.seq) ||
          frame.seq < 0 ||
          frame.seq < lastSeq
        ) {
          return;
        }
        lastSeq = frame.seq;
        lastReducedState = frame;
        if (frame.state?.connection_ready === true && frame.state.connection_unavailable === null) finish();
      } catch (error) {
        finish(error instanceof Error ? error : new Error(String(error)));
      }
    };
    await promise;
  } catch (err) {
    const signal = AbortSignal.timeout(5_000);
    const [replay, sessionsBody] = await Promise.all([
      fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`, { signal })
        .then((res) => res.json())
        .catch(() => null),
      fetch(`${baseUrl}/api/sessions`, { signal })
        .then((res) => res.json())
        .catch(() => null),
    ]);
    const frames: unknown[] = Array.isArray(replay) ? replay : (replay?.frames ?? []);
    const summary = JSON.stringify(
      {
        sessionId,
        lastReducedState,
        sessions: sessionsBody,
        startupError: findStartupError(frames),
        framesCount: frames.length,
        firstFrames: frames.slice(0, 5),
      },
      null,
      2,
    );
    throw new Error(
      `waitForAcpReady failed: ${err instanceof Error ? err.message : String(err)}\n` +
        `Diagnostic snapshot:\n${summary}\n` +
        `--- debug.log tail ---\n${debugLogTail(home)}\n--- end debug.log ---`,
      { cause: err },
    );
  }
}

/** Poll the replay log until it contains any (or, with `mode: "all"`, every) needle. */
export async function waitForReplayContains(
  baseUrl: string,
  sessionId: string,
  needles: string | string[],
  options: { timeoutMs?: number; mode?: "any" | "all" } = {},
): Promise<void> {
  const list = Array.isArray(needles) ? needles : [needles];
  await expect
    .poll(
      async () => {
        const json = JSON.stringify(await replayFrames(baseUrl, sessionId));
        return options.mode === "all" ? list.every((n) => json.includes(n)) : list.some((n) => json.includes(n));
      },
      { timeout: options.timeoutMs ?? 15_000, intervals: [100, 200, 500, 1000] },
    )
    .toBe(true);
}

export async function spawnAcpAgentAndWait(
  baseUrl: string,
  sessionId: string,
  timeoutMs = 30_000,
  home?: string,
): Promise<void> {
  await spawnAcpAgent(baseUrl, sessionId);
  await waitForAcpReady(baseUrl, sessionId, timeoutMs, home);
}

/** Explicitly spawn the agent so its ACP session is attached before a test drives it. */
export async function spawnAcpAgent(baseUrl: string, sessionId: string, agent?: string): Promise<void> {
  const res = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/spawn`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ agent }),
  });
  if (![200, 202, 409].includes(res.status)) throw new Error(`structured view spawn failed: ${res.status}`);
}

export async function postPrompt(baseUrl: string, sessionId: string, text: string): Promise<Response> {
  return postAcp(baseUrl, sessionId, "/prompt", { text });
}

export async function sessionIdByTitle(baseUrl: string, title: string): Promise<string> {
  const session = (await listSessions(baseUrl)).find((s) => s.title === title);
  if (!session) throw new Error(`seeded session '${title}' missing`);
  return session.id;
}

type SpawnServe = (opts?: ServeOptions) => Promise<ServeHandle>;
type AcpSessionOptions = ServeOptions & { title: string; tool?: string };

/** Seed an unstarted structured session with a genuine first-creation selection. */
export async function seedAcpSession(
  spawnServe: SpawnServe,
  { title, tool, ...opts }: AcpSessionOptions,
): Promise<{ serve: ServeHandle; sessionId: string }> {
  const serve = await spawnServe({
    acp: true,
    seedFn: seedSessionViaAoeAdd({ title, tool, structuredView: true }),
    ...opts,
  });
  return { serve, sessionId: await sessionIdByTitle(serve.baseUrl, title) };
}

/** Seed the first conversation, then spawn its Native and wait for readiness. */
export async function startAcpSession(spawnServe: SpawnServe, opts: AcpSessionOptions) {
  const started = await seedAcpSession(spawnServe, opts);
  await spawnAcpAgentAndWait(started.serve.baseUrl, started.sessionId, 30_000, started.serve.home);
  return started;
}

export async function replayJson(baseUrl: string, sessionId: string): Promise<string> {
  return JSON.stringify(await replayFrames(baseUrl, sessionId));
}

export async function postAcp(baseUrl: string, sessionId: string, path: string, body?: unknown): Promise<Response> {
  return fetch(`${baseUrl}/api/sessions/${sessionId}/acp${path}`, {
    method: "POST",
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
}

export const chunk = (text: string) => ({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
/** Parks the fake agent's turn until `releaseTurn` or a cancel. */
export const HOLD = { sessionUpdate: "wait_for_release" };
export const endTurn = (...updates: object[]) => ({ updates, stopReason: "end_turn" });
export const script = (...turns: object[]) => ({ turns });

/** Release the fake agent's `wait_for_release` gate for a server spawned with a script object. */
export function releaseTurn(serve: ServeHandle): void {
  writeFileSync(`${fakeAcpScriptPath(serve.home)}.release`, "release");
}

/** The composer textbox in either its idle or mid-turn state. */
export function composer(page: Page): Locator {
  return page.getByRole("textbox", { name: /Send a message|Queue a follow-up/i });
}

export function idleComposer(page: Page): Locator {
  return page.getByRole("textbox", { name: /Send a message/i });
}

export function stopButton(page: Page): Locator {
  return page.getByTestId("composer-actions").getByRole("button", { name: "Stop" });
}

export async function waitForStructuredView(page: Page, timeoutMs = 15_000): Promise<void> {
  await expect(composer(page)).toBeVisible({ timeout: timeoutMs });
}

export async function openSession(page: Page, serve: ServeHandle, sessionId: string): Promise<void> {
  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
}

/** Navigate to a structured view session and optionally submit a prompt from its composer. */
export async function openStructuredView(
  page: Page,
  serve: ServeHandle,
  sessionId: string,
  prompt?: string,
): Promise<Locator> {
  await openSession(page, serve, sessionId);
  await waitForStructuredView(page);
  const box = composer(page);
  if (prompt !== undefined) {
    await box.fill(prompt);
    await page.getByRole("button", { name: "Send message", exact: true }).click();
  }
  return box;
}

/** The `<select>` sibling of a FormFields label. */
export function settingsSelectByLabel(page: Page, labelText: string): Locator {
  return page.locator("label").filter({ hasText: labelText }).locator("xpath=..").locator("select").first();
}

export async function openSettingsTab(page: Page, label: string): Promise<void> {
  await page.getByRole("button", { name: label, exact: true }).click();
}

/** Wait until the Profile select resolves, so the profile-driven settings refetch cannot clobber an edit. */
export async function waitForSettingsLoaded(page: Page): Promise<void> {
  const profileSelect = settingsSelectByLabel(page, "Profile");
  await expect(profileSelect).toBeVisible({ timeout: 10_000 });
  await expect.poll(async () => (await profileSelect.inputValue()).length, { timeout: 10_000 }).toBeGreaterThan(0);
}

/** Attach the daemon debug.log tail and fake-acp.log; call before the server's HOME is deleted. */
export async function attachServeDiagnostics(testInfo: TestInfo, serve: { home: string }): Promise<void> {
  const attach = (name: string, read: () => string) => {
    let body: string;
    try {
      body = read();
    } catch (e) {
      return testInfo.attach(`${name}.read-error`, { body: String(e), contentType: "text/plain" });
    }
    return testInfo.attach(name, { body, contentType: "text/plain" });
  };
  const debugLog = debugLogPath(serve.home);
  if (debugLog) await attach("debug.log", () => tail(readFileSync(debugLog, "utf8"), 64_000));
  else
    await testInfo.attach("debug.log.missing", { body: `no debug.log under ${serve.home}`, contentType: "text/plain" });
  const fakeLog = join(serve.home, "fake-acp.log");
  if (existsSync(fakeLog)) await attach("fake-acp.log", () => readFileSync(fakeLog, "utf8"));
  else await testInfo.attach("fake-acp.log.missing", { body: `expected at ${fakeLog}`, contentType: "text/plain" });
}
