import { expect, test } from "bun:test";
import { approvalController, ensurePrivateDirectories, integrationDirectory, nativeApprovalKey, observeApproval, observeAskDialog, validateControl, validateReply } from "../omp-extension/index.ts";
import { chmodSync, mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const questions = [{ id: "q1", question: "Choose", options: [{ label: "one" }, { label: "two" }] }];
const answer = { kind: "submit" as const, results: [{ id: "q1", question: "Choose", options: ["one", "two"], multi: false, selectedOptions: ["two"], timedOut: false }] };
const snapshot = {
  schema_version: 1 as const,
  instance_id: "instance-a",
  provider_session_id: "session-a",
  writer_pid: 123,
  revision: 7,
  status: "waiting" as const,
  reply_keys: [{ kind: "named" as const, value: "F12" as const }],
  prompt: { kind: "question" as const, id: "prompt-a", title: "Choose", questions },
};
const validReply = {
  schema_version: 1,
  instance_id: "instance-a",
  provider_session_id: "session-a",
  writer_pid: 123,
  revision: 7,
  prompt_id: "prompt-a",
  response_id: "7d442dd2-7b10-4cc4-9207-123456789abc",
  response: { kind: "question", answers: [{ id: "q1", selected_options: ["two"] }] },
};

test("external question answer closes only the child dialog and returns a native submit result", async () => {
  let accept!: (value: typeof answer) => boolean;
  let nativeSettled = false;
  let closed = false;
  const parent = new AbortController();
  const dialog = observeAskDialog((_questions, options) => {
    const { promise, reject } = Promise.withResolvers<typeof answer | undefined>();
    options?.signal?.addEventListener("abort", () => {
      nativeSettled = true;
      reject(new Error("AbortError"));
    }, { once: true });
    return promise;
  }, {
    open(_id, _questions, submit) { accept = submit as (value: typeof answer) => boolean; },
    close() { closed = true; },
  });
  const result = dialog(questions, { signal: parent.signal });
  expect(accept(answer)).toBe(true);
  expect(await result).toEqual(answer);
  expect(nativeSettled).toBe(true);
  expect(parent.signal.aborted).toBe(false);
  expect(closed).toBe(true);
  expect(accept(answer)).toBe(false);
});

test("native question completion and cancellation win over a later sidebar answer", async () => {
  for (const nativeResult of [answer, undefined]) {
    let accept!: (value: typeof answer) => boolean;
    const dialog = observeAskDialog(async () => nativeResult, {
      open(_id, _questions, submit) { accept = submit as (value: typeof answer) => boolean; },
      close() {},
    });
    await expect(dialog(questions)).resolves.toEqual(nativeResult);
    expect(accept(answer)).toBe(false);
  }
});
test("outer tool cancellation propagates into the native dialog", async () => {
  let childSignal: AbortSignal | undefined;
  const parent = new AbortController();
  const dialog = observeAskDialog((_questions, options) => {
    childSignal = options?.signal;
    const { promise, resolve } = Promise.withResolvers<typeof answer | undefined>();
    childSignal?.addEventListener("abort", () => resolve(undefined), { once: true });
    return promise;
  }, { open() {}, close() {} });
  const result = dialog(questions, { signal: parent.signal });
  parent.abort();
  await expect(result).resolves.toBeUndefined();
  expect(parent.signal.aborted).toBe(true);
  expect(childSignal?.aborted).toBe(true);
});

test("rejects stale, wrong-session, wrong-prompt, and invalid question envelopes", () => {
  expect(validateReply(validReply, snapshot)).toBe(true);
  const variants = [
    { ...validReply, instance_id: "other" },
    { ...validReply, provider_session_id: "other" },
    { ...validReply, writer_pid: 456 },
    { ...validReply, revision: 6 },
    { ...validReply, prompt_id: "other" },
    { ...validReply, response: { kind: "question", answers: [{ id: "other", selected_options: ["two"] }] } },
    { ...validReply, response: { kind: "question", answers: [{ id: "q1", selected_options: ["not offered"] }] } },
    { ...validReply, response: { kind: "question", answers: [{ id: "q1", selected_options: [] }] } },
    { ...validReply, response: { kind: "question", answers: [{ id: "q1", selected_options: ["two", "two"] }] } },
    { ...validReply, response: { kind: "question", answers: [{ id: "q1", selected_options: ["two"], custom_input: "other" }] } },
    { ...validReply, response: { kind: "question", answers: [{ id: "q1", selected_options: [], custom_input: " " }] } },
  ];
  for (const value of variants) expect(validateReply(value, snapshot)).toBe(false);
});
test("allow_always replies require native prompt support", () => {
  const approvalSnapshot = {
    ...snapshot,
    prompt: { kind: "approval" as const, id: "approval-a", title: "Approve", allow_always: false },
  };
  const allowAlwaysReply = {
    ...validReply,
    prompt_id: "approval-a",
    response: { kind: "approval", choice: "allow_always" },
  };
  expect(validateReply(allowAlwaysReply, approvalSnapshot)).toBe(false);
  expect(validateReply(allowAlwaysReply, {
    ...approvalSnapshot,
    prompt: { ...approvalSnapshot.prompt, allow_always: true },
  })).toBe(true);
});

test("approval observer preserves the exact native result and key mapping is native", async () => {
  const nativeResult = false;
  const events: string[] = [];
  const result = await observeApproval(async () => nativeResult, () => events.push("open"), () => events.push("close"));
  expect(result).toBe(nativeResult);
  expect(events).toEqual(["open", "close"]);
  expect(nativeApprovalKey("allow", ["ctrl+enter", "ctrl+q"])).toBe("C-Enter");
  expect(nativeApprovalKey("allow", ["ctrl+q"])).toBe("C-q");
  expect(nativeApprovalKey("deny", ["ctrl+c", "escape"])).toBe("Escape");
  expect(nativeApprovalKey("allow_always", [])).toBeUndefined();
});
test("provider creates owner-only directories and uses the shared /tmp hook path", async () => {
  const uid = process.getuid();
  const scratch = mkdtempSync(path.join(tmpdir(), "aoe-omp-integration-"));
  const root = path.join(scratch, `aoe-hooks-${uid}`);
  const dir = path.join(root, "instance-a");
  try {
    expect(await ensurePrivateDirectories(root, dir, uid)).toBe(true);
    expect(statSync(root).mode & 0o777).toBe(0o700);
    expect(statSync(dir).mode & 0o777).toBe(0o700);
    expect(integrationDirectory(uid, "instance-a")).toBe(`/tmp/aoe-hooks-${uid}/instance-a`);
    chmodSync(root, 0o1700);
    expect(await ensurePrivateDirectories(root, dir, uid)).toBe(false);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

test("approval controls restore a safe prior policy and cannot negate force-on launch flags", () => {
  for (const initial of ["always-ask", "write", "yolo"] as const) {
    let mode = initial as "always-ask" | "write" | "yolo";
    const setting = { get: () => mode, override(_settings: unknown, value: typeof mode) { mode = value; } };
    const control = approvalController(setting, {}, false);
    control.set(true);
    expect(control.state()).toEqual({ enabled: true, reversible: true });
    control.set(false);
    expect(mode).toBe(initial === "yolo" ? "always-ask" : initial);
    const forced = approvalController(setting, {}, true);
    expect(() => forced.set(false)).toThrow("restart");
    expect(forced.state()).toEqual({ enabled: true, reversible: false });
  }
});

test("policy control requests bind identity, writer, revision, nonce and boolean intent", () => {
  const control = { schema_version: 1, instance_id: snapshot.instance_id, provider_session_id: snapshot.provider_session_id,
    writer_pid: snapshot.writer_pid, revision: snapshot.revision, request_id: validReply.response_id, enabled: true };
  expect(validateControl(control, snapshot)).toBe(true);
  for (const patch of [{ instance_id: "other" }, { provider_session_id: "other" }, { writer_pid: 456 },
    { revision: 6 }, { request_id: "invalid" }, { enabled: "on" }]) {
    expect(validateControl({ ...control, ...patch }, snapshot)).toBe(false);
  }
});
