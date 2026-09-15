import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import type * as NativeTimers from "node:timers/promises";
import { expect, it, vi } from "vitest";
import { appDirFor, spawnAoeServe, type ServeHandle } from "./aoeServe";

vi.mock("node:timers/promises", async (importOriginal) => {
  const timers = await importOriginal<typeof NativeTimers>();
  return { ...timers, setTimeout: vi.fn(timers.setTimeout) };
});

it.skipIf(process.platform === "win32")(
  "stop retains a leaderless runner group until descendants exit, including after a deadline",
  async () => {
    const root = mkdtempSync(join(tmpdir(), "aoe-stop-test-"));
    const binary = join(root, "aoe");
    const release = join(root, "release");
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
