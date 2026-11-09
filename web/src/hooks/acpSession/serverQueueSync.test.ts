import { describe, expect, it } from "vitest";
import type { ServerQueuedPrompt } from "../../lib/api";
import {
  confirmServerQueue,
  mutateServerQueue,
  reconcileQueueSnapshot,
  serverQueueRevision,
  subscribeServerQueue,
  trackServerQueueEnqueue,
} from "./serverQueueSync";

const row = (id: string, text = id): ServerQueuedPrompt => ({ id, text, seq: 1, created_at: "t" });
function held() {
  let release!: () => void;
  const promise = new Promise<void>((resolve) => {
    release = resolve;
  });
  return { promise, release };
}

describe("shared queue reconciliation", () => {
  it("keeps newly enqueued rows outside the pending clear's original ids", async () => {
    const id = "clear-new";
    const unsubscribe = subscribeServerQueue(id, () => {});
    const clear = held();
    const request = mutateServerQueue(id, { kind: "clear", ids: ["old"] }, () => clear.promise);
    expect(reconcileQueueSnapshot(id, serverQueueRevision(id), [row("old"), row("new")])).toEqual([row("new")]);
    clear.release();
    await request;
    unsubscribe();
  });

  it("does not overlay an older pending edit after a newer edit has saved", async () => {
    const id = "edits-out-of-order";
    const unsubscribe = subscribeServerQueue(id, () => {});
    const first = held();
    const second = held();
    const a = mutateServerQueue(id, { kind: "edit", id: "row", text: "A" }, () => first.promise);
    const b = mutateServerQueue(id, { kind: "edit", id: "row", text: "B" }, () => second.promise);
    expect(reconcileQueueSnapshot(id, serverQueueRevision(id), [row("row")])).toEqual([row("row", "B")]);
    second.release();
    await b;
    expect(reconcileQueueSnapshot(id, serverQueueRevision(id), [row("row", "B")])).toEqual([row("row", "B")]);
    first.release();
    await a;
    unsubscribe();
  });

  it("shares an in-flight enqueue across remounts until captured removal settles", async () => {
    const id = "enqueue-remount-clear";
    const enqueue = held();
    const events: string[] = [];
    const pending = trackServerQueueEnqueue(id, "row", async () => {
      events.push("enqueue");
      await enqueue.promise;
      return row("row");
    });
    const repeated = trackServerQueueEnqueue(id, "row", async () => {
      events.push("duplicate enqueue");
      return row("row");
    });
    const clear = mutateServerQueue(id, { kind: "clear", ids: ["row"] }, async () => {
      events.push("clear");
    });
    expect(events).toEqual(["enqueue"]);
    enqueue.release();
    await Promise.all([pending, repeated, clear]);
    expect(events).toEqual(["enqueue", "clear"]);
  });

  it("invalidates reads started before an enqueue confirmation", () => {
    const id = "enqueue-read";
    const unsubscribe = subscribeServerQueue(id, () => {});
    const revision = serverQueueRevision(id);
    confirmServerQueue(id);
    expect(reconcileQueueSnapshot(id, revision, [])).toBeNull();
    expect(reconcileQueueSnapshot(id, serverQueueRevision(id), [row("confirmed")])).toEqual([row("confirmed")]);
    unsubscribe();
  });

  it("does not recreate a retired session when a cancelled snapshot lands", () => {
    const id = "unmounted-read";
    const unsubscribe = subscribeServerQueue(id, () => {});
    const revision = serverQueueRevision(id);
    unsubscribe();
    expect(reconcileQueueSnapshot(id, revision, [row("late")])).toBeNull();
  });
});
