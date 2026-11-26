import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { appDirFor, listSessions, resolveAoeBinary, seedSessionViaAoeAdd } from "../helpers/aoeServe";

const nativeId = "11111111-2222-4333-8444-555555555555";
interface AgentEvent {
  event: string;
  text?: string;
  argv?: string[];
}

for (const scenario of [
  { name: "idle", status: "idle" },
  { name: "waiting", status: "waiting" },
  { name: "running", status: "running" },
  { name: "profile", status: "idle", otherProfile: true },
  { name: "failed-resume", status: "running", fail: true },
]) {
  test(`startup recovery ${scenario.name} resumes without typing into the agent`, async ({ spawnServe }) => {
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
    log.write(json.dumps(dict(event=event, **data)) + "\\n")
record("ready", argv=sys.argv[1:])
print("FAKE_AGENT_READY", flush=True)
deadline = time.monotonic() + 6
if ${scenario.fail ? "True" : "False"}:
    sys.exit(42)
pending = b""
while True:
    timeout = max(0, deadline - time.monotonic()) if deadline else None
    readable, _, _ = select.select([sys.stdin], [], [], timeout)
    if not readable:
        record("window_closed")
        deadline = None
        continue
    chunk = os.read(sys.stdin.fileno(), 4096)
    if not chunk:
        break
    pending += chunk
    while b"\\n" in pending:
        line, pending = pending.split(b"\\n", 1)
        record("prompt", text=line.decode().rstrip("\\r"))
`,
          { mode: 0o755 },
        );
        if (scenario.otherProfile) {
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
    const events = (): AgentEvent[] => {
      if (!existsSync(log)) return [];
      return readFileSync(log, "utf8")
        .trim()
        .split("\n")
        .filter(Boolean)
        .map((line) => JSON.parse(line) as AgentEvent);
    };
    await expect.poll(() => events().some((event) => event.event === "ready"), { timeout: 20_000 }).toBe(true);
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
    expect(events().find((event) => event.event === "ready")?.argv).toContain(nativeId);
    expect(events().find((event) => event.event === "ready")?.argv).toContain("--resume");
    // Observe the full input window to catch late extra prompts.
    await expect.poll(() => events().some((event) => event.event === "window_closed"), { timeout: 15_000 }).toBe(true);
    expect(
      events()
        .filter((event) => event.event === "prompt")
        .map((event) => event.text),
    ).toEqual([]);

    if (scenario.name === "idle") {
      // Neither restart path types anything into the relaunched agent.
      for (const source of ["api", "cli"]) {
        const offset = events().length;
        if (source === "cli") {
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
            body: "{}",
          });
          expect(response.status, await response.text()).toBe(200);
        }
        await expect
          .poll(() =>
            events()
              .slice(offset)
              .some((event) => event.event === "ready"),
          )
          .toBe(true);
        await expect
          .poll(
            () =>
              events()
                .slice(offset)
                .some((event) => event.event === "window_closed"),
            { timeout: 15_000 },
          )
          .toBe(true);
        expect(
          events()
            .slice(offset)
            .filter((event) => event.event === "prompt")
            .map((event) => event.text),
        ).toEqual([]);
      }
    }
  });
}
