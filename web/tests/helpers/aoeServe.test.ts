import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import type * as NativeTimers from "node:timers/promises";
import { expect, it, vi } from "vitest";
import { appDirFor, spawnAoeServe, type ServeHandle } from "./aoeServe";

/** A stand-in `aoe` that only answers the harness readiness probe. */
function writeFakeAoe(root: string): string {
  const binary = join(root, "aoe");
  writeFileSync(
    binary,
    `#!${process.execPath}
const http = require("node:http");
const port = Number(process.argv[process.argv.indexOf("--port") + 1]);
http.createServer((_, response) => response.end("{}")).listen(port, "127.0.0.1", () => {
  console.log("http://127.0.0.1:" + port + "/");
});
`,
    { mode: 0o700 },
  );
  return binary;
}

vi.mock("node:timers/promises", async (importOriginal) => {
  const timers = await importOriginal<typeof NativeTimers>();
  return { ...timers, setTimeout: vi.fn(timers.setTimeout) };
});

it.skipIf(process.platform === "win32")(
  "stop retains a leaderless runner group until descendants exit, including after a deadline",
  async () => {
    const root = mkdtempSync(join(tmpdir(), "aoe-stop-test-"));
    const binary = writeFakeAoe(root);
    const release = join(root, "release");
    const descendant = `
const fs = require("node:fs");
setInterval(() => { if (fs.existsSync(${JSON.stringify(release)})) process.exit(0); }, 10);
setTimeout(() => process.exit(1), 15000);
console.log(process.pid);
`;
    const leader = spawn(
      process.execPath,
      [
        "-e",
        `const { spawn } = require("node:child_process");
const child = spawn(process.execPath, ["-e", ${JSON.stringify(descendant)}], { stdio: ["ignore", "pipe", "inherit"] });
child.stdout.once("data", data => process.stdout.write(data, () => process.exit(0)));`,
      ],
      { detached: true, stdio: ["ignore", "pipe", "pipe"] },
    );
    const leaderExit = once(leader, "exit", { signal: AbortSignal.timeout(5000) });
    let serve: ServeHandle | undefined;
    let stop: Promise<void> | undefined;
    const nativeTimers = await vi.importActual<typeof NativeTimers>("node:timers/promises");
    const now = performance.now.bind(performance);
    let clockOffset = 0;
    const liveMembers = () => {
      const result = spawnSync("ps", ["-axo", "pid=,pgid=,stat="], { encoding: "utf8", timeout: 2000 });
      if (result.status !== 0) throw new Error(`ps failed: ${result.error ?? result.stderr}`);
      return result.stdout.split("\n").flatMap((line) => {
        const [pid, group, state] = line.trim().split(/\s+/);
        return Number(group) === leader.pid && !state.startsWith("Z") ? [Number(pid)] : [];
      });
    };
    try {
      const [output] = await once(leader.stdout!, "data", { signal: AbortSignal.timeout(5000) });
      const childPid = Number(output.toString().trim());
      await leaderExit;
      expect(liveMembers()).toEqual([childPid]);
      vi.stubEnv("AOE_E2E_BINARY", binary);
      serve = await spawnAoeServe({ workerIndex: 0, parallelIndex: 0 });
      const workers = join(appDirFor(serve.home, join(serve.home, "config"), binary), "acp-workers");
      mkdirSync(workers);
      const record = JSON.stringify({
        pid: leader.pid,
        session_id: "departed-runner",
        socket_path: join(workers, "departed-runner.sock"),
        generation: 123,
      });
      writeFileSync(join(workers, "departed-runner.json"), record);
      vi.spyOn(performance, "now").mockImplementation(() => now() + clockOffset);

      for (const expire of [true, false]) {
        let polls = 0;
        const draining = Promise.withResolvers<void>();
        vi.mocked(delay).mockImplementation(async (ms) => {
          // Re-entry proves stop has kept observing the still-live descendant.
          // Native processes need real polling; only the failure deadline is advanced.
          if (++polls === 2) draining.resolve();
          await nativeTimers.setTimeout(ms);
        });
        let settled = false;
        stop = serve.stop();
        const outcome = stop.then(
          () => {
            settled = true;
          },
          (error: unknown) => {
            settled = true;
            return error;
          },
        );
        await Promise.race([
          draining.promise,
          outcome.then((error) => {
            throw error ?? new Error("stop returned before the descendant exited");
          }),
        ]);
        expect(settled).toBe(false);
        expect(existsSync(serve.home)).toBe(true);
        expect(readdirSync(workers).some((name) => readFileSync(join(workers, name), "utf8") === record)).toBe(true);
        expect(liveMembers()).toEqual([childPid]);
        if (expire) {
          clockOffset += 25_000;
          expect(await outcome).toBeInstanceOf(AggregateError);
          expect(existsSync(serve.home)).toBe(true);
          expect(readdirSync(workers).some((name) => readFileSync(join(workers, name), "utf8") === record)).toBe(true);
          expect(liveMembers()).toEqual([childPid]);
        } else {
          writeFileSync(release, "");
          await stop;
          expect(liveMembers()).toEqual([]);
          expect(existsSync(serve.home)).toBe(false);
        }
      }
    } finally {
      writeFileSync(release, "");
      await stop?.catch(() => {});
      vi.restoreAllMocks();
      vi.unstubAllEnvs();
      await expect.poll(liveMembers, { timeout: 5000 }).toEqual([]);
      if (serve && existsSync(serve.home)) await serve.stop();
      rmSync(root, { recursive: true, force: true });
    }
  },
  20_000,
);

// A runner can load its record just before teardown revokes it and save it back
// just after (a late `mark_detached`). Its watchdog then keeps matching the record
// and never exits unless the harness revokes the resaved record too.
it.skipIf(process.platform !== "linux")(
  "stop revokes a runner record saved back after revocation",
  async () => {
    const root = mkdtempSync(join(tmpdir(), "aoe-stop-test-"));
    const binary = writeFakeAoe(root);
    let serve: ServeHandle | undefined;
    let runnerPid: number | undefined;
    try {
      vi.stubEnv("AOE_E2E_BINARY", binary);
      serve = await spawnAoeServe({ workerIndex: 0, parallelIndex: 0 });
      const workers = join(appDirFor(serve.home, join(serve.home, "config"), binary), "acp-workers");
      mkdirSync(workers);
      const sessionId = "resaved-runner";
      const recordPath = join(workers, `${sessionId}.json`);
      const socketPath = join(workers, `${sessionId}.sock`);
      // Titled like `aoe __acp-runner` so the harness accepts it as this record's live runner.
      const title = `${realpathSync(binary)} __acp-runner --socket ${socketPath} --session-id ${sessionId} --generation 7 -- agent`;
      const runner = spawn(
        process.execPath,
        [
          "-e",
          `const fs = require("node:fs");
process.title = ${JSON.stringify(title)};
fs.writeFileSync(${JSON.stringify(join(root, "runner-ready"))}, "");
const path = ${JSON.stringify(recordPath)};
let saved, resaved = false, missing = 0;
setInterval(() => {
  if (fs.existsSync(path)) {
    if (!saved) {
      saved = fs.readFileSync(path);
      fs.writeFileSync(${JSON.stringify(join(root, "runner-record-loaded"))}, "");
    }
    missing = 0;
    return;
  }
  if (saved && !resaved) { resaved = true; fs.writeFileSync(path + ".tmp", saved); fs.renameSync(path + ".tmp", path); return; }
  if (saved && ++missing >= 2) process.exit(0);
}, 20);`,
          "x".repeat(title.length),
        ],
        { detached: true, stdio: "ignore" },
      );
      runnerPid = runner.pid!;
      // The harness matches the runner by its `ps` command line, so the record
      // must not exist before the child has renamed itself.
      await expect
        .poll(() => existsSync(join(root, "runner-ready")), { timeout: 10_000, message: "fake runner renamed itself" })
        .toBe(true);
      writeFileSync(
        recordPath,
        JSON.stringify({ pid: runnerPid, session_id: sessionId, socket_path: socketPath, generation: 7 }),
      );
      // The resave only happens if the runner read the record before teardown revokes it.
      await expect
        .poll(() => existsSync(join(root, "runner-record-loaded")), {
          timeout: 10_000,
          message: "fake runner loaded its record",
        })
        .toBe(true);
      const exited = once(runner, "exit");

      const stop = serve.stop();
      await expect(
        Promise.race([
          stop,
          delay(10_000).then(() => {
            throw new Error("stop did not revoke the resaved record");
          }),
        ]),
      ).resolves.toBeUndefined();
      await exited;
      expect(existsSync(serve.home)).toBe(false);
    } finally {
      if (runnerPid) {
        try {
          process.kill(-runnerPid, "SIGKILL");
        } catch {
          // already exited
        }
      }
      vi.unstubAllEnvs();
      if (serve && existsSync(serve.home)) await serve.stop().catch(() => {});
      rmSync(root, { recursive: true, force: true });
    }
  },
  20_000,
);
