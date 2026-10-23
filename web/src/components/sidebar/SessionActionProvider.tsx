import { useLayoutEffect, useReducer, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import {
  attachSessionProject,
  setSessionColor,
  setSessionNotifications,
  type RuntimeCursor,
  type SessionMutation,
  type SessionReceipt,
  type SessionSettingOutcome,
} from "../../lib/api";
import type { SessionResponse } from "../../lib/types";
import { reportError } from "../../lib/toastBus";
import { AddProjectModal } from "./AddProjectModal";
import { detectNotifyPreset } from "./rowModel";
import {
  SessionActionContext,
  sessionActionTarget,
  type SessionActionTarget,
  type SessionActionsOwner,
  type SettingValues,
} from "./usePendingSetting";

type Kind = keyof SettingValues;
type Value = SettingValues[Kind];
interface PendingSetting {
  kind: Kind;
  targets: SessionActionTarget[];
  value: Value;
  from: Value;
  sent: Value[];
  receipts: SessionReceipt[] | null;
  invalidated: boolean;
  surrendered: boolean;
}

const targetKey = (target: SessionActionTarget) =>
  JSON.stringify([target.epoch, target.id, target.profile, target.createdAt]);
const batchKey = (kind: Kind, targets: SessionActionTarget[]) => JSON.stringify([kind, targets.map(targetKey).sort()]);
const observed = (pending: PendingSetting, rows: Record<string, RuntimeCursor>) =>
  pending.receipts !== null &&
  pending.receipts.every(({ id, cursor }) => rows[id]?.epoch === cursor.epoch && rows[id].revision >= cursor.revision);
const canonicalSetting = (record: PendingSetting, sessions: SessionResponse[]): Value => {
  if (record.kind === "color") {
    for (const target of record.targets) {
      const row = sessions.find((row) => row.id === target.id);
      if (row?.color != null) return row.color;
    }
    return null;
  }
  const row = sessions.find((row) => row.id === record.targets[0]?.id);
  return detectNotifyPreset([row?.notify_on_waiting, row?.notify_on_idle, row?.notify_on_error]);
};

/** Own only attach results and the two accepted setting queues, above visual grouping and routes. */
export function SessionActionProvider({
  children,
  sessions,
  runtimeEpoch,
  observedById,
  onSessionMutation,
  readOnly = false,
  available = true,
}: {
  children: ReactNode;
  sessions: SessionResponse[];
  runtimeEpoch: string | null;
  observedById: Record<string, RuntimeCursor>;
  onSessionMutation: (mutation: SessionMutation) => void;
  readOnly?: boolean;
  available?: boolean;
}) {
  const authority = useRef({ sessions, runtimeEpoch, observedById, onSessionMutation, readOnly, available });
  const alive = useRef(true);
  const pending = useRef(new Map<string, PendingSetting>());
  const tails = useRef(new Map<string, Promise<void>>());
  const accepted = useRef(new Set<PendingSetting>());
  const [, changed] = useReducer((value: number) => value + 1, 0);
  const [attach, setAttach] = useState<{
    target: SessionActionTarget;
    title: string;
  } | null>(null);
  const attachWork = useRef<{
    target: SessionActionTarget;
    invalidated: boolean;
    finished: boolean;
  } | null>(null);
  const sameIdentity = (target: SessionActionTarget) => {
    const current = authority.current;
    return (
      alive.current &&
      target.epoch !== "" &&
      current.runtimeEpoch === target.epoch &&
      current.sessions.some(
        (row) => row.id === target.id && row.profile === target.profile && row.created_at === target.createdAt,
      )
    );
  };
  const canSubmit = (target: SessionActionTarget) =>
    authority.current.available && !authority.current.readOnly && sameIdentity(target);
  useLayoutEffect(() => {
    authority.current = { sessions, runtimeEpoch, observedById, onSessionMutation, readOnly, available };
    for (const record of accepted.current) {
      if (!record.targets.every(sameIdentity)) record.invalidated = true;
    }
    const attachment = attachWork.current;
    if (attachment && !attachment.finished && !sameIdentity(attachment.target)) attachment.invalidated = true;
    let removed = false;
    for (const [key, record] of pending.current) {
      if (!record.surrendered && record.receipts === null) {
        const server = canonicalSetting(record, sessions);
        if (!Object.is(server, record.from) && !record.sent.some((value) => Object.is(value, server))) {
          record.surrendered = true;
          removed = true;
        }
      }
      if (observed(record, observedById)) {
        pending.current.delete(key);
        removed = true;
      }
    }
    if (removed) changed();
  });
  useLayoutEffect(() => {
    alive.current = true;
    const acceptedRecords = accepted.current;
    const pendingRecords = pending.current;
    const queuedTails = tails.current;
    return () => {
      alive.current = false;
      for (const record of acceptedRecords) record.invalidated = true;
      pendingRecords.clear();
      queuedTails.clear();
    };
  }, []);

  const active = (kind: Kind, targets: SessionActionTarget[], server: Value) => {
    const record = pending.current.get(batchKey(kind, targets));
    if (!record || record.invalidated || record.surrendered || observed(record, authority.current.observedById))
      return null;
    if (
      record.receipts === null &&
      !Object.is(server, record.from) &&
      !record.sent.some((value) => Object.is(value, server))
    )
      return null;
    return record;
  };
  const owner: SessionActionsOwner = {
    openAttach: (session) => {
      const target = sessionActionTarget(session, authority.current.runtimeEpoch);
      if (!canSubmit(target)) return reportError("That session changed or is unavailable. Please try again.");
      attachWork.current = { target, invalidated: false, finished: false };
      setAttach({ target, title: session.title });
    },
    value: (kind, targets, server) => {
      const record = active(kind, targets, server);
      return (record ? record.value : server) as typeof server;
    },
    set: (kind, selected, next, server) => {
      if (selected.length === 0 || !selected.every(canSubmit)) {
        reportError("That session changed or is unavailable. No setting was submitted.");
        return;
      }
      const previous = active(kind, selected, server);
      if (Object.is(next, previous ? previous.value : server)) return;
      const targets = selected.map((target) => ({ ...target }));
      const key = batchKey(kind, targets);
      const sent = [...(previous?.sent ?? [])];
      if (!sent.some((value) => Object.is(value, next))) sent.push(next);
      const record: PendingSetting = {
        kind,
        targets,
        value: next,
        from: previous ? previous.from : server,
        sent,
        receipts: null,
        invalidated: false,
        surrendered: false,
      };
      pending.current.set(key, record);
      accepted.current.add(record);
      changed();
      const lanes = targets.map((target) => `${kind}:${targetKey(target)}`);
      const before = lanes.map((lane) => tails.current.get(lane) ?? Promise.resolve());
      const task = Promise.all(before)
        .then(async () => {
          const results = await Promise.all(
            targets.map(async (target): Promise<SessionSettingOutcome> => {
              if (record.invalidated || !sameIdentity(target)) return { outcome: "uncertain" };
              if (authority.current.readOnly) return { outcome: "rejected" };
              const result =
                kind === "color"
                  ? await setSessionColor(target.id, next as SettingValues["color"], target.epoch)
                  : await setSessionNotifications(target.id, next as SettingValues["notifications"], target.epoch);
              if (record.invalidated || !sameIdentity(target)) return { outcome: "uncertain" };
              if (result.outcome === "committed") authority.current.onSessionMutation(result.mutation);
              return result;
            }),
          );
          if (!alive.current || pending.current.get(key) !== record) return;
          if (
            results.every(
              (result): result is Extract<SessionSettingOutcome, { outcome: "committed" }> =>
                result.outcome === "committed",
            )
          ) {
            record.receipts = results.map((result) => ({
              id: result.mutation.session.id,
              cursor: result.mutation.cursor,
            }));
          } else {
            pending.current.delete(key);
            reportError(
              results.some((result) => result.outcome === "uncertain")
                ? "The setting outcome is unconfirmed. Check canonical state before retrying."
                : `Could not change ${kind === "color" ? "the session color" : "notifications"}. Please try again.`,
            );
          }
          changed();
        })
        .catch(() => {
          if (alive.current && pending.current.get(key) === record) {
            pending.current.delete(key);
            reportError("The setting outcome is unconfirmed. Check canonical state before retrying.");
            changed();
          }
        });
      for (const lane of lanes) tails.current.set(lane, task);
      void task.finally(() => {
        accepted.current.delete(record);
        for (const lane of lanes) if (tails.current.get(lane) === task) tails.current.delete(lane);
      });
    },
  };
  return (
    <SessionActionContext.Provider value={owner}>
      {children}
      {attach &&
        createPortal(
          <AddProjectModal
            title={attach.title}
            profile={attach.target.profile || undefined}
            onCancel={() => setAttach(null)}
            onDone={() => setAttach(null)}
            onSubmit={async (project, attachExistingBranch) => {
              const target = attach.target;
              const operation = attachWork.current;
              if (!operation || operation.target !== target || operation.invalidated || !canSubmit(target))
                return { ok: false, message: "Session identity or runtime changed. Nothing was submitted." };
              const result = await attachSessionProject(target.id, project, {
                attachExistingBranch,
                epoch: target.epoch,
              });
              if (operation.invalidated || !sameIdentity(target))
                return {
                  ok: false,
                  message: "Session identity or runtime changed. Attachment outcome is unconfirmed.",
                };
              if (result.ok && result.session && result.cursor) {
                authority.current.onSessionMutation({ session: result.session, cursor: result.cursor });
              }
              if (result.ok) operation.finished = true;
              return result;
            }}
          />,
          document.body,
        )}
    </SessionActionContext.Provider>
  );
}
