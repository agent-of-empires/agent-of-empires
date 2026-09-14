import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";
import {
  mkdtemp,
  mkdir,
  readdir,
  readFile,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { z } from "zod";

// Real ACP server and provider serializer; the local endpoint never calls a model.
test(
  "native conversation boundaries survive restart, late prompts, and storage failures",
  { timeout: 30000 },
  async () => {
    const dir = await mkdtemp(join(tmpdir(), "aoe-rpc-"));
    const captures: unknown[] = [];
    let releaseOld = () => {};
    let oldStarted = () => {};
    const oldRequest = new Promise<void>((resolve) => {
      oldStarted = resolve;
    });
    const oldResponse = new Promise<void>((resolve) => {
      releaseOld = resolve;
    });
    const server = createServer(async (req, res) => {
      let raw = "";
      for await (const chunk of req) raw += chunk;
      const body = z
        .object({ messages: z.array(z.unknown()) })
        .parse(JSON.parse(raw));
      captures.push(body.messages);
      if (raw.includes("held-A")) {
        oldStarted();
        await oldResponse;
      }
      res.writeHead(200, { "content-type": "text/event-stream" });
      const events = [
        {
          type: "message_start",
          message: {
            id: "msg_test",
            type: "message",
            role: "assistant",
            model: "claude-test",
            content: [],
            stop_reason: null,
            stop_sequence: null,
            usage: { input_tokens: 1, output_tokens: 0 },
          },
        },
        {
          type: "content_block_start",
          index: 0,
          content_block: { type: "text", text: "" },
        },
        {
          type: "content_block_delta",
          index: 0,
          delta: { type: "text_delta", text: "reply" },
        },
        { type: "content_block_stop", index: 0 },
        {
          type: "message_delta",
          delta: { stop_reason: "end_turn", stop_sequence: null },
          usage: { output_tokens: 1 },
        },
        { type: "message_stop" },
      ];
      for (const event of events)
        res.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
      res.end();
    });
    await new Promise<void>((resolve) =>
      server.listen(0, "127.0.0.1", resolve),
    );
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const providerUrl = `http://127.0.0.1:${address.port}/v1`;
    const children: ChildProcessWithoutNullStreams[] = [];
    function worker(artifactDir: string | null = dir, cwd = dir) {
      const env: NodeJS.ProcessEnv = {
        ...process.env,
        AOE_AGENT_MODEL: "claude-test",
        ANTHROPIC_API_KEY: "isolated-test",
        ANTHROPIC_BASE_URL: providerUrl,
      };
      delete env.AOE_ARTIFACT_DIR;
      if (artifactDir !== null) env.AOE_ARTIFACT_DIR = artifactDir;
      const child = spawn(
        process.execPath,
        [
          "--experimental-strip-types",
          new URL("../src/index.ts", import.meta.url).pathname,
        ],
        { env, cwd, stdio: ["pipe", "pipe", "pipe"] },
      );
      children.push(child);
      let stderr = "";
      child.stderr.on("data", (chunk) => {
        stderr += chunk;
      });
      let id = 0;
      const pending = new Map<
        number,
        { resolve: (value: unknown) => void; reject: (error: Error) => void }
      >();
      createInterface({ input: child.stdout }).on("line", (line) => {
        const msg = z
          .object({
            id: z.number().optional(),
            result: z.unknown().optional(),
            error: z.unknown().optional(),
          })
          .parse(JSON.parse(line));
        if (msg.id === undefined) return;
        const call = pending.get(msg.id);
        if (!call) return;
        pending.delete(msg.id);
        if (msg.error) call.reject(new Error(JSON.stringify(msg.error)));
        else call.resolve(msg.result);
      });
      child.on("exit", () => {
        for (const call of pending.values())
          call.reject(new Error(`agent exited: ${stderr}`));
        pending.clear();
      });
      const rpc = (method: string, params: unknown) =>
        new Promise<unknown>((resolve, reject) => {
          pending.set(++id, { resolve, reject });
          child.stdin.write(
            JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n",
          );
        });
      return {
        init: () =>
          rpc("initialize", { protocolVersion: 1, clientCapabilities: {} }),
        new: async () =>
          z
            .object({ sessionId: z.string() })
            .parse(await rpc("session/new", { cwd, mcpServers: [] })).sessionId,
        load: (sessionId: string) =>
          rpc("session/load", { sessionId, cwd, mcpServers: [] }),
        prompt: (sessionId: string, text: string) =>
          rpc("session/prompt", {
            sessionId,
            prompt: [{ type: "text", text }],
          }),
        stop: async (signal: "SIGTERM" | "SIGKILL" = "SIGTERM") => {
          const exited = new Promise<void>((resolve) =>
            child.once("exit", () => resolve()),
          );
          child.kill(signal);
          await exited;
        },
      };
    }
    const providerText = () => JSON.stringify(captures.at(-1));
    try {
      let w = worker();
      await w.init();
      const a = await w.new();
      await w.prompt(a, "A-private");
      await w.stop();
      w = worker();
      await w.init();
      await w.load(a);
      await w.prompt(a, "normal-resume");
      assert.ok(
        providerText().includes("A-private"),
        "ordinary resume retains A",
      );
      const held = w.prompt(a, "held-A");
      await oldRequest;
      const b = await w.new();
      await w.prompt(b, "B-private");
      assert.equal(
        providerText().includes("A-private"),
        false,
        "in-memory clear",
      );
      releaseOld();
      await held;
      await w.prompt(a, "late-A");
      await w.stop();
      w = worker();
      await w.init();
      await w.load(b);
      await w.prompt(b, "resume-B");
      assert.equal(
        providerText().includes("A-private"),
        false,
        "resumed B must not contain A",
      );
      assert.equal(providerText().includes("held-A"), false);
      assert.equal(providerText().includes("late-A"), false);
      assert.equal(providerText().includes("B-private"), true);
      const empty = await w.new();
      await w.stop("SIGKILL");
      w = worker();
      await w.init();
      await w.load(empty);
      await w.prompt(empty, "first-after-empty-clear");
      assert.deepEqual(captures.at(-1), [
        {
          role: "user",
          content: [{ type: "text", text: "first-after-empty-clear" }],
        },
      ]);
      await assert.rejects(w.load("f".repeat(32)), /ENOENT/);
      await assert.rejects(w.load("../escape"), /session ID/);
      // A clear onto a non-writable artifact dir is best-effort: session/new
      // still returns a usable, ephemeral session and does not disturb the one
      // already loaded. A later load of the ephemeral id misses its file and
      // resets context, so nothing leaks.
      await rename(dir, `${dir}-held`);
      let blockedClear = "";
      try {
        await writeFile(dir, "blocked");
        blockedClear = await w.new();
      } finally {
        await rm(dir, { force: true });
        await rename(`${dir}-held`, dir);
      }
      await w.prompt(blockedClear, "started-despite-blocked");
      assert.ok(
        providerText().includes("started-despite-blocked"),
        "a clear onto a non-writable artifact dir still starts a session",
      );
      await w.prompt(empty, "after-blocked-clear");
      assert.ok(
        providerText().includes("after-blocked-clear"),
        "the previously loaded session stays usable",
      );
      await w.stop();

      for (const differentCwd of [false, true]) {
        const isolated = join(dir, `isolated-${differentCwd}`);
        await mkdir(isolated);
        w = worker(isolated, differentCwd ? isolated : dir);
        await w.init();
        await assert.rejects(w.load(b), /ENOENT/);
        const id = await w.new();
        await w.prompt(id, "other-instance");
        assert.equal(providerText().includes("B-private"), false);
        await w.stop();
      }

      const legacy = join(dir, "legacy");
      await mkdir(legacy);
      const legacyBytes =
        '{"role":"user","content":"legacy-secret"}\n{"role":"assistant","content":"old-reply"}\n';
      await writeFile(join(legacy, "transcript.jsonl"), legacyBytes);
      w = worker(legacy);
      await w.init();
      await assert.rejects(w.load("a".repeat(32)), /ENOENT/);
      const fresh = await w.new();
      await w.stop();
      w = worker(legacy);
      await w.init();
      await w.load(fresh);
      await w.prompt(fresh, "after-upgrade");
      assert.equal(providerText().includes("legacy-secret"), false);
      assert.equal(
        await readFile(join(legacy, "transcript.jsonl"), "utf8"),
        legacyBytes,
      );
      await w.stop();

      const blocked = join(dir, "not-a-directory");
      await writeFile(blocked, "blocked");
      w = worker(blocked);
      await w.init();
      // A non-directory artifact path cannot hold a transcript, but the
      // best-effort create still lets the session start and run ephemerally.
      const startedOnFile = await w.new();
      await w.prompt(startedOnFile, "runs-without-usable-artifact-dir");
      assert.ok(providerText().includes("runs-without-usable-artifact-dir"));
      await w.stop();

      const probe = join(dir, "probe");
      await mkdir(probe);
      w = worker(null, probe);
      const capabilities = z
        .object({ agentCapabilities: z.object({ loadSession: z.boolean() }) })
        .parse(await w.init());
      assert.equal(capabilities.agentCapabilities.loadSession, false);
      const ephemeral = await w.new();
      await assert.rejects(w.load(ephemeral), /persistence/);
      assert.deepEqual(await readdir(probe), []);
      await w.stop();
    } finally {
      releaseOld();
      await Promise.all(
        children
          .filter(
            (child) => child.exitCode === null && child.signalCode === null,
          )
          .map(
            (child) =>
              new Promise<void>((resolve) => {
                child.once("exit", () => resolve());
                child.kill();
              }),
          ),
      );
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      await rm(dir, { recursive: true, force: true });
    }
  },
);
