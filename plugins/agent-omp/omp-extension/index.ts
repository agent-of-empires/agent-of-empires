import { randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import { closeSync, constants, fstatSync, lstatSync, openSync, readFileSync } from "node:fs";
import { mkdir, open, rename, unlink } from "node:fs/promises";
import path from "node:path";
import type { ExtensionAPI, ExtensionAskDialogQuestion, ExtensionAskDialogResult, ExtensionUIDialogOptions } from "@oh-my-pi/pi-coding-agent/extensibility/extensions";

type Question = { id: string; question: string; options: { label: string; description?: string }[]; multi?: boolean; recommended?: number };
type Prompt =
  | { kind: "approval"; id: string; title: string; allow_always: boolean }
  | { kind: "question"; id: string; title: string; questions: Question[] };
type Snapshot = {
  schema_version: 1; instance_id: string; provider_session_id: string; writer_pid: number;
  revision: number; status: "running" | "waiting" | "idle" | "error"; reply_keys: string[]; prompt: Prompt | null;
};
type Reply = {
  schema_version: 1; instance_id: string; provider_session_id: string; writer_pid: number;
  revision: number; prompt_id: string; response_id: string;
  response: { kind: "approval"; choice: "allow" | "deny" | "allow_always" } | { kind: "question"; answers: { id: string; selected_options: string[]; custom_input?: string }[] };
};
type AskState = { id: string; questions: ExtensionAskDialogQuestion[]; accept: (result: ExtensionAskDialogResult) => boolean };
type ApprovalState = { id: string; title: string; replyable: boolean };

const SNAPSHOT_NAME = "integration-omp.agent-of-empires.json";
const REPLY_NAME = "integration-omp.agent-of-empires.reply.json";
const HEARTBEAT_MS = 5_000;
const MAX_REPLY_BYTES = 1_048_576;
const KEY = "F12";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}

export function validateReply(value: unknown, snapshot: Snapshot): value is Reply {
  if (!isRecord(value) || value.schema_version !== 1 || value.instance_id !== snapshot.instance_id ||
      value.provider_session_id !== snapshot.provider_session_id || value.writer_pid !== snapshot.writer_pid ||
      value.revision !== snapshot.revision || value.prompt_id !== snapshot.prompt?.id ||
      typeof value.response_id !== "string" || !UUID.test(value.response_id) || !isRecord(value.response)) return false;
  const response = value.response;
  if (snapshot.prompt?.kind === "approval") {
    if (response.kind !== "approval" || (response.choice !== "allow" && response.choice !== "allow_always" && response.choice !== "deny")) return false;
    return response.choice !== "allow_always" || snapshot.prompt.allow_always;
  }
  if (snapshot.prompt?.kind !== "question" || response.kind !== "question" || !Array.isArray(response.answers) ||
      response.answers.length !== snapshot.prompt.questions.length) return false;
  return snapshot.prompt.questions.every((question, index) => {
    const answer = response.answers[index];
    if (!isRecord(answer) || answer.id !== question.id || !Array.isArray(answer.selected_options) ||
        !answer.selected_options.every(option => typeof option === "string" && question.options.some(item => item.label === option)) ||
        new Set(answer.selected_options).size !== answer.selected_options.length) return false;
    if (answer.custom_input !== undefined && (typeof answer.custom_input !== "string" || !answer.custom_input.trim())) return false;
    if (question.multi) return true;
    const hasOption = answer.selected_options.length === 1;
    const hasCustom = typeof answer.custom_input === "string" && answer.custom_input.trim().length > 0;
    return hasOption !== hasCustom;
  });
}

function resolveInstanceId(): string | undefined {
  const direct = process.env.AOE_INSTANCE_ID?.trim();
  if (direct && /^[A-Za-z0-9_-]{1,128}$/.test(direct)) return direct;
  const pane = process.env.TMUX_PANE;
  if (!pane || !/^%[0-9]+$/.test(pane)) return undefined;
  try {
    const result = execFileSync("tmux", ["show-environment", "-h", "-t", pane, "AOE_INSTANCE_ID"], { encoding: "utf8", timeout: 1000 }).trim();
    const value = result.startsWith("AOE_INSTANCE_ID=") ? result.slice("AOE_INSTANCE_ID=".length) : "";
    return /^[A-Za-z0-9_-]{1,128}$/.test(value) ? value : undefined;
  } catch { return undefined; }
}

function questionPrompt(id: string, questions: ExtensionAskDialogQuestion[]): Prompt {
  return { kind: "question", id, title: questions.map(q => q.question).join("\n"), questions: questions.map(q => ({
    id: q.id, question: q.question,
    options: q.options.map(({ label, description }) => description === undefined ? { label } : { label, description }),
    ...(q.multi === undefined ? {} : { multi: q.multi }), ...(q.recommended === undefined ? {} : { recommended: q.recommended }),
  })) };
}

function safeDirectorySync(dir: string, uid: number): boolean {
  try {
    const stat = lstatSync(dir);
    return stat.isDirectory() && !stat.isSymbolicLink() && stat.uid === uid && (stat.mode & 0o7777) === 0o700;
  } catch { return false; }
}

function readReply(file: string, uid: number, current: Snapshot): Reply | undefined {
  let descriptor: number | undefined;
  try {
    descriptor = openSync(file, constants.O_RDONLY | constants.O_NOFOLLOW);
    const stat = fstatSync(descriptor);
    if (!stat.isFile() || stat.uid !== uid || (stat.mode & 0o7777) !== 0o600 || stat.size > MAX_REPLY_BYTES) return undefined;
    const value: unknown = JSON.parse(readFileSync(descriptor, "utf8"));
    return validateReply(value, current) ? value : undefined;
  } catch { return undefined; }
  finally { if (descriptor !== undefined) closeSync(descriptor); }
}
async function ownsSnapshot(file: string, uid: number, instanceId: string, sessionId: string): Promise<boolean> {
  let descriptor: number | undefined;
  try {
    descriptor = openSync(file, constants.O_RDONLY | constants.O_NOFOLLOW);
    const stat = fstatSync(descriptor);
    if (!stat.isFile() || stat.uid !== uid || (stat.mode & 0o7777) !== 0o600 || stat.size > MAX_REPLY_BYTES) return false;
    const value: unknown = JSON.parse(readFileSync(descriptor, "utf8"));
    return isRecord(value) && value.instance_id === instanceId && value.provider_session_id === sessionId && value.writer_pid === process.pid;
  } catch { return false; }
  finally { if (descriptor !== undefined) closeSync(descriptor); }
}
export async function ensurePrivateDirectories(root: string, dir: string, uid: number): Promise<boolean> {
  for (const directory of [root, dir]) {
    try { await mkdir(directory, { mode: 0o700 }); }
    catch (error) { if (!isRecord(error) || error.code !== "EEXIST") return false; }
    if (!safeDirectorySync(directory, uid)) return false;
  }
  return true;
}

export function integrationDirectory(uid: number, instanceId: string): string {
  return path.join("/tmp", `aoe-hooks-${uid}`, instanceId);
}
type AskDialog = (questions: ExtensionAskDialogQuestion[], options?: ExtensionUIDialogOptions) => Promise<ExtensionAskDialogResult | undefined>;
type AskObserver = {
  open: (id: string, questions: ExtensionAskDialogQuestion[], accept: (result: ExtensionAskDialogResult) => boolean) => void;
  close: (id: string) => void;
};

export function observeAskDialog(nativeAsk: AskDialog, observer: AskObserver): AskDialog {
  return async (questions, options) => {
    const id = randomUUID();
    const controller = new AbortController();
    const childSignal = options?.signal ? AbortSignal.any([options.signal, controller.signal]) : controller.signal;
    const { promise: external, resolve: resolveExternal } = Promise.withResolvers<ExtensionAskDialogResult>();
    const { promise: nativeSettled, resolve: settleNative } = Promise.withResolvers<void>();
    let pending = true;
    let externalAnswer = false;
    const accept = (result: ExtensionAskDialogResult): boolean => {
      if (!pending || externalAnswer) return false;
      externalAnswer = true;
      pending = false;
      controller.abort();
      void nativeSettled.then(() => resolveExternal(result));
      return true;
    };
    try { observer.open(id, questions, accept); } catch { /* Observation must not disrupt the native dialog. */ }
    let native: Promise<ExtensionAskDialogResult | undefined>;
    try { native = Promise.resolve(nativeAsk(questions, { ...options, signal: childSignal })); }
    catch (error) {
      pending = false;
      try { observer.close(id); } catch { /* Preserve the native exception. */ }
      throw error;
    }
    native.then(() => settleNative(), () => settleNative());
    try {
      return await Promise.race([native.then(result => {
        if (externalAnswer) return external;
        pending = false;
        return result;
      }, error => {
        if (externalAnswer) return external;
        pending = false;
        throw error;
      }), external]);
    } finally {
      pending = false;
      try { observer.close(id); } catch { /* Observation must not replace the native result. */ }
    }
  };
}
export async function observeApproval<T>(nativeApproval: () => Promise<T>, onOpen: () => void, onClose: () => void): Promise<T> {
  try { onOpen(); } catch { /* Observation must not change the native decision. */ }
  try {
    return await nativeApproval();
  } finally {
    try { onClose(); } catch { /* Observation must not change the native decision. */ }
  }
}

export function nativeApprovalKey(choice: "allow" | "deny" | "allow_always", bindings: readonly string[]): string | undefined {
  if (choice === "allow_always") return undefined;
  const orderedBindings = choice === "deny"
    ? [...bindings.filter(binding => binding.toLowerCase().endsWith("escape")),
      ...bindings.filter(binding => !binding.toLowerCase().endsWith("escape"))]
    : bindings;
  for (const binding of orderedBindings) {
    const parts = binding.toLowerCase().split("+");
    const key = parts.pop();
    if (!key) continue;
    const modifiers: string[] = [];
    let supported = true;
    for (const modifier of parts) {
      if (modifier === "ctrl") modifiers.push("C");
      else if (modifier === "alt") modifiers.push("M");
      else if (modifier === "shift") modifiers.push("S");
      else supported = false;
    }
    if (!supported || !/^(?:[a-z0-9]|enter|return|escape|esc|tab|space|up|down|left|right|f(?:[1-9]|1[0-2]))$/.test(key)) continue;
    const names: Record<string, string> = { return: "Enter", esc: "Escape", space: "Space" };
    const name = names[key] ?? (/^[a-z0-9]$/.test(key) ? key : key[0]!.toUpperCase() + key.slice(1));
    return [...modifiers, name].join("-");
  }
  return undefined;
}


export default function (pi: ExtensionAPI): void {
  const instanceId = resolveInstanceId();
  if (!instanceId || typeof process.getuid !== "function") return;
  const uid = process.getuid();
  const root = path.join("/tmp", `aoe-hooks-${uid}`);
  const dir = integrationDirectory(uid, instanceId);
  const snapshotPath = path.join(dir, SNAPSHOT_NAME);
  const replyPath = path.join(dir, REPLY_NAME);
  let sessionId = "";
  let revision = 0;
  let prompt: Prompt | null = null;
  let status: Snapshot["status"] = "idle";
  let heartbeat: ReturnType<typeof setInterval> | undefined;
  let unlisten: (() => void) | undefined;
  let currentAsk: AskState | undefined;
  let activeApproval: ApprovalState | undefined;
  let approvalQueue: { id: string; title: string }[] = [];
  let approvalBindings: string[] = [];
  let cancelBindings: string[] = [];
  let handledPromptId: string | undefined;

  const currentSnapshot = (): Snapshot => ({ schema_version: 1, instance_id: instanceId, provider_session_id: sessionId,
    writer_pid: process.pid, revision, status, reply_keys: [KEY], prompt });
  let writeQueue = Promise.resolve();
  const writeSnapshot = (): Promise<void> => {
    writeQueue = writeQueue.then(async () => {
      let temporary: string | undefined;
      try {
        if (!(await ensurePrivateDirectories(root, dir, uid))) return;
        temporary = path.join(dir, `.integration-omp.${process.pid}.${randomUUID()}.tmp`);
        const file = await open(temporary, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY | constants.O_NOFOLLOW, 0o600);
        try { await file.writeFile(JSON.stringify(currentSnapshot()), "utf8"); await file.sync(); } finally { await file.close(); }
        await rename(temporary, snapshotPath);
        temporary = undefined;
        try { const directory = await open(dir, constants.O_RDONLY); await directory.sync(); await directory.close(); } catch { /* best effort */ }
      } catch {
        if (temporary) await unlink(temporary).catch(() => undefined);
      }
    });
    return writeQueue;
  };
  const publish = (nextPrompt: Prompt | null, nextStatus: Snapshot["status"]): void => {
    if (prompt?.id !== nextPrompt?.id) handledPromptId = undefined;
    if (JSON.stringify(prompt) !== JSON.stringify(nextPrompt) || status !== nextStatus) revision++;
    prompt = nextPrompt; status = nextStatus;
    void writeSnapshot();
  };

  const handleReply = (reply: Reply): boolean => {
    if (handledPromptId === reply.prompt_id) return false;
    if (reply.response.kind === "question" && currentAsk?.id === reply.prompt_id) {
      const ask = currentAsk;
      const result: ExtensionAskDialogResult = { kind: "submit", results: reply.response.answers.map(answer => {
        const question = ask.questions.find(item => item.id === answer.id)!;
        return { id: question.id, question: question.question, options: question.options.map(option => option.label),
          multi: question.multi ?? false, selectedOptions: answer.selected_options, customInput: answer.custom_input, timedOut: false };
      }) };
      if (!ask.accept(result)) return false;
      handledPromptId = reply.prompt_id;
      unlink(replyPath).catch(() => undefined);
      return true;
    }
    if (reply.response.kind !== "approval" || !activeApproval?.replyable || activeApproval.id !== reply.prompt_id) return false;
    const bindings = reply.response.choice === "allow" ? approvalBindings : cancelBindings;
    const key = nativeApprovalKey(reply.response.choice, bindings);
    const pane = process.env.TMUX_PANE;
    if (!key || !pane || !/^%[0-9]+$/.test(pane)) return false;
    try { execFileSync("tmux", ["send-keys", "-t", pane, key], { timeout: 1000, stdio: "ignore" }); }
    catch { return false; }
    handledPromptId = reply.prompt_id;
    unlink(replyPath).catch(() => undefined);
    return true;
  };

  pi.on("session_start", async (_event, ctx) => {
    sessionId = ctx.sessionManager.getSessionId();
    try {
      const [{ appApproveKeys }, { getKeybindings }] = await Promise.all([
        import("@oh-my-pi/pi-tui/keybinding-matchers"),
        import("@oh-my-pi/pi-tui/keybindings"),
      ]);
      approvalBindings = [...appApproveKeys()];
      cancelBindings = [...getKeybindings().getKeys("tui.select.cancel")];
    } catch { /* Native approval remains available when key observation fails. */ }
    const ui = ctx.ui;
    if (ui.askDialog) {
      const nativeAsk = ui.askDialog.bind(ui);
      ui.askDialog = observeAskDialog(nativeAsk, {
        open(id, questions, accept) {
          currentAsk = { id, questions, accept };
          publish(questionPrompt(id, questions), "waiting");
        },
        close(id) {
          if (currentAsk?.id === id) currentAsk = undefined;
          publish(activeApproval?.replyable ? { kind: "approval", id: activeApproval.id, title: activeApproval.title, allow_always: false } : null,
            activeApproval ? "waiting" : "running");
        },
      });
    }
    if (ui.approve) {
      const nativeApprove = ui.approve.bind(ui);
      ui.approve = (approval, options) => {
        const paired = approvalQueue.length === 1 ? approvalQueue.shift() : undefined;
        const approvalId = paired?.id ?? randomUUID();
        const title = paired?.title ?? approval.compact;
        activeApproval = { id: approvalId, title, replyable: !!paired };
        return observeApproval(
          () => nativeApprove(approval, options),
          () => publish(paired ? { kind: "approval", id: approvalId, title, allow_always: false } : null, "waiting"),
          () => {
            if (activeApproval?.id === approvalId) activeApproval = undefined;
            publish(currentAsk ? questionPrompt(currentAsk.id, currentAsk.questions) : null, currentAsk ? "waiting" : "running");
          },
        );
      };
    }
    unlisten = ui.onTerminalInput(data => {
      if (data !== "\u001b[24~" || !safeDirectorySync(root, uid) || !safeDirectorySync(dir, uid)) return;
      const reply = readReply(replyPath, uid, currentSnapshot());
      if (!reply) return;
      if (handledPromptId === reply.prompt_id) {
        unlink(replyPath).catch(() => undefined);
        return { consume: true };
      }
      return handleReply(reply) ? { consume: true } : undefined;
    });
    await writeSnapshot();
    heartbeat = setInterval(() => void writeSnapshot(), HEARTBEAT_MS);
  });

  pi.on("tool_approval_requested", event => {
    if (event.sessionId === sessionId) approvalQueue.push({ id: event.toolCallId, title: event.reason ?? event.toolName });
  });
  pi.on("tool_approval_resolved", event => {
    if (event.sessionId === sessionId) approvalQueue = approvalQueue.filter(item => item.id !== event.toolCallId);
  });
  pi.on("agent_start", () => publish(prompt, "running"));
  pi.on("agent_end", () => { if (prompt === null) publish(null, "idle"); });
  pi.on("session_shutdown", async () => {
    clearInterval(heartbeat);
    unlisten?.();
    await writeQueue;
    if (safeDirectorySync(root, uid) && safeDirectorySync(dir, uid) &&
        await ownsSnapshot(snapshotPath, uid, instanceId, sessionId)) {
      await unlink(snapshotPath).catch(() => undefined);
    }
  });
}
