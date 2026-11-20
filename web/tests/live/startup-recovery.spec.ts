import { spawnSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { appDirFor, listSessions, resolveAoeBinary, seedSessionViaAoeAdd } from "../helpers/aoeServe";

const defaultMessage = "wake up: pick up what you were doing";
const nativeId = "11111111-2222-4333-8444-555555555555";
interface AgentEvent {
  event: string;
  pid: number;
  text?: string;
  argv?: string[];
  pending_hex?: string;
}

function readAgentEvents(log: string, offset = 0): AgentEvent[] {
  if (!existsSync(log)) return [];
  const text = readFileSync(log, "utf8");
  const end = text.lastIndexOf("\n");
  if (end < 0) return [];
  return text
    .slice(0, end)
    .split("\n")
    .map((line) => JSON.parse(line) as AgentEvent)
    .slice(offset);
}

interface NativeReady {
  event: AgentEvent;
  index: number;
}

async function waitForReady(events: () => AgentEvent[]): Promise<NativeReady> {
  let ready: NativeReady | undefined;
  await expect
    .poll(
      () => {
        const snapshot = events();
        const index = snapshot.findIndex((event) => event.event === "ready");
        if (index < 0) return false;
        ready = { event: snapshot[index]!, index };
        return true;
      },
      { timeout: 20_000 },
    )
    .toBe(true);
  return ready!;
}

async function expectInputWindow(events: () => AgentEvent[], ready: NativeReady, expected: string[]): Promise<void> {
  const payloads = (snapshot: AgentEvent[]) =>
    snapshot
      .filter((event, index) => index > ready.index && event.pid === ready.event.pid && event.event === "prompt")
      .map((event) => event.text);
  if (expected.length > 0) {
    await expect.poll(() => payloads(events()), { timeout: 20_000 }).toEqual(expected);
  }
  let snapshot: AgentEvent[] = [];
  let closed: AgentEvent | undefined;
  await expect
    .poll(
      () => {
        const current = events();
        let lastPrompt = ready.index;
        current.forEach((event, index) => {
          if (event.pid === ready.event.pid && event.event === "prompt" && index > lastPrompt) lastPrompt = index;
        });
        let index = -1;
        current.forEach((event, candidate) => {
          if (candidate > lastPrompt && event.pid === ready.event.pid && event.event === "window_closed")
            index = candidate;
        });
        if (index < 0) return false;
        snapshot = current;
        closed = current[index];
        return true;
      },
      { timeout: 20_000 },
    )
    .toBe(true);
  expect(snapshot[ready.index]).toEqual(ready.event);
  expect(payloads(snapshot)).toEqual(expected);
  expect(snapshot.filter((event) => event.event === "prompt" && event.pid !== ready.event.pid)).toEqual([]);
  expect(snapshot.slice(0, ready.index + 1).filter((event) => event.event === "prompt")).toEqual([]);
  expect(closed?.pid).toBe(ready.event.pid);
  expect(closed?.pending_hex).toBe("");
}

for (const scenario of [
  { name: "omitted", status: "idle", config: "", prompts: [] },
  { name: "false", status: "waiting", config: "startup_recovery_wake = false", prompts: [] },
  { name: "enabled", status: "running", config: "startup_recovery_wake = true", prompts: [defaultMessage] },
  {
    name: "custom",
    status: "idle",
    config: 'startup_recovery_wake = true\nrestart_wake_message = "continue exactly here"',
    prompts: ["continue exactly here"],
  },
  {
    name: "empty",
    status: "waiting",
    config: 'startup_recovery_wake = true\nrestart_wake_message = ""',
    prompts: [],
  },
  {
    name: "profile-on",
    status: "idle",
    config: "startup_recovery_wake = false",
    profileConfig: 'startup_recovery_wake = true\nrestart_wake_message = "profile wake"',
    prompts: ["profile wake"],
  },
  {
    name: "profile-off",
    status: "waiting",
    config: "startup_recovery_wake = true",
    profileConfig: "startup_recovery_wake = false",
    prompts: [],
  },
  {
    name: "failed-resume",
    status: "running",
    config: "startup_recovery_wake = true",
    fail: true,
    prompts: [],
  },
]) {
  test(`startup recovery ${scenario.name} respects launch and wake policy`, async ({ spawnServe }) => {
    const title = `recovery-${scenario.name}`;
    let sessionFile = "";
    const serve = await spawnServe({
      seedFn: (seed) => {
        const appDir = appDirFor(seed.home, seed.xdg, resolveAoeBinary());
        const log = join(seed.home, "agent-input.jsonl");
        writeFileSync(
          join(seed.shimBin, "claude"),
          `#!/usr/bin/env python3
import json, os, select, sys, time
log = open(${JSON.stringify(log)}, "a", buffering=1)
def record(event, **data):
    log.write(json.dumps(dict(event=event, pid=os.getpid(), **data)) + "\\n")
record("ready", argv=sys.argv[1:])
print("FAKE_AGENT_READY", flush=True)
deadline = time.monotonic() + 12
if ${scenario.fail ? "True" : "False"}:
    sys.exit(42)
pending = b""
while True:
    timeout = max(0, deadline - time.monotonic()) if deadline else None
    readable, _, _ = select.select([sys.stdin], [], [], timeout)
    if not readable:
        record("window_closed", pending_hex=pending.hex())
        deadline = None
        continue
    chunk = os.read(sys.stdin.fileno(), 4096)
    if not chunk:
        break
    pending += chunk
    deadline = time.monotonic() + 1
    while b"\\n" in pending:
        line, pending = pending.split(b"\\n", 1)
        record("prompt", text=line.decode().rstrip("\\r"))
`,
          { mode: 0o755 },
        );
        if (scenario.profileConfig) {
          const created = spawnSync(resolveAoeBinary(), ["profile", "create", "recovery-other"], {
            env: seed.env,
            encoding: "utf8",
          });
          expect(created.status, created.stderr).toBe(0);
          seedSessionViaAoeAdd({ title })({
            ...seed,
            env: { ...seed.env, AGENT_OF_EMPIRES_PROFILE: "recovery-other" },
          });
        } else {
          seedSessionViaAoeAdd({ title })(seed);
        }
        const transcriptDir = join(
          seed.home,
          ".claude",
          "projects",
          join(seed.home, "project").replace(/[^a-zA-Z0-9-]/g, "-"),
        );
        mkdirSync(transcriptDir, { recursive: true });
        writeFileSync(join(transcriptDir, `${nativeId}.jsonl`), "{}\n");
        appendFileSync(join(appDir, "config.toml"), `\n[session]\n${scenario.config}\n`);
        const profilesDir = join(appDir, "profiles");
        let seeded = false;
        for (const profile of readdirSync(profilesDir)) {
          const path = join(profilesDir, profile, "sessions.json");
          if (!existsSync(path)) continue;
          const data = JSON.parse(readFileSync(path, "utf8"));
          const row = data.find((row: { title: string }) => row.title === title);
          if (!row) continue;
          row.status = scenario.status;
          row.agent_session_id = nativeId;
          writeFileSync(path, JSON.stringify(data));
          sessionFile = path;
          if (scenario.profileConfig) {
            writeFileSync(join(profilesDir, profile, "config.toml"), `[session]\n${scenario.profileConfig}\n`);
          }
          seeded = true;
        }
        expect(seeded, "a persisted native-resume candidate must exist before boot").toBe(true);
        // Keep the private server reachable after a rejected resume loses its only agent pane.
        const probe = spawnSync(
          "tmux",
          ["-S", seed.env.AOE_TMUX_SOCKET!, "new-session", "-d", "-s", "recovery-probe", "exec cat"],
          { env: seed.env, encoding: "utf8" },
        );
        expect(probe.status, probe.stderr).toBe(0);
      },
    });
    const log = join(serve.home, "agent-input.jsonl");
    const events = () => readAgentEvents(log);
    const ready = await waitForReady(events);
    const [session] = await listSessions(serve.baseUrl);
    expect(session?.title).toBe(title);
    expect(session?.lifecycle_generation).toBeGreaterThan(0);
    const name = `${serve.tmuxPrefix}${title}_${session!.id.slice(0, 8)}`;
    if (scenario.fail) {
      await expect
        .poll(
          () => {
            const [row] = JSON.parse(readFileSync(sessionFile, "utf8"));
            return {
              failed: row.resume_probe_failed_sid,
              nativeId: row.agent_session_id,
              reserved: row.lifecycle_reservation ?? null,
            };
          },
          { timeout: 20_000 },
        )
        .toEqual({ failed: nativeId, nativeId, reserved: null });
      expect(spawnSync("tmux", ["-S", serve.tmuxSocket, "has-session", "-t", name]).status).not.toBe(0);
      expect(events().filter((event) => event.event === "prompt")).toEqual([]);
      return;
    }
    expect(spawnSync("tmux", ["-S", serve.tmuxSocket, "has-session", "-t", name]).status).toBe(0);
    expect(ready.event.argv).toContain(nativeId);
    expect(ready.event.argv).toContain("--resume");
    await expectInputWindow(events, ready, scenario.prompts);

    if (scenario.name === "omitted" || scenario.name === "false") {
      const requests =
        scenario.name === "omitted"
          ? [
              { source: "api", body: {}, prompts: [defaultMessage] },
              { source: "api", body: { wake_message: "explicit custom wake" }, prompts: ["explicit custom wake"] },
              { source: "cli", body: {}, prompts: [defaultMessage] },
            ]
          : [{ source: "api", body: { wake_message: "" }, prompts: [] }];
      for (const request of requests) {
        const offset = events().length;
        if (request.source === "cli") {
          const restart = spawnSync(resolveAoeBinary(), ["session", "restart", session!.id], {
            env: serve.env,
            timeout: 20_000,
            encoding: "utf8",
          });
          expect(restart.status, restart.stderr).toBe(0);
        } else {
          const response = await fetch(`${serve.baseUrl}/api/sessions/${session!.id}/restart`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(request.body),
          });
          expect(response.status, await response.text()).toBe(200);
        }
        const phaseEvents = () => readAgentEvents(log, offset);
        const phaseReady = await waitForReady(phaseEvents);
        await expectInputWindow(phaseEvents, phaseReady, request.prompts);
      }
    }
  });
}
