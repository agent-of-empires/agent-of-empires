import type { PluginUiEntry } from "../../lib/api";

export type ComposerDraftOperation =
  | { kind: "insert-text"; text: string }
  | { kind: "replace-selection"; text: string; captureId?: string }
  | { kind: "set-text"; text: string };

export interface BrowserVoiceAnchor {
  expectedText: string;
  selectionStart: number;
  selectionEnd: number;
}

interface ScopedBrowserVoiceAnchor extends BrowserVoiceAnchor {
  pluginId: string;
  actionId: string;
  sessionId: string;
  expiresAt: number;
  expiryTimer: ReturnType<typeof setTimeout>;
}

const BROWSER_VOICE_ANCHOR_TTL_MS = 10 * 60 * 1000;
const MAX_BROWSER_VOICE_ANCHORS = 128;
const browserVoiceAnchors = new Map<string, ScopedBrowserVoiceAnchor>();

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function str(obj: Record<string, unknown>, key: string): string | undefined {
  const v = obj[key];
  return typeof v === "string" ? v : undefined;
}

function sweepExpiredBrowserVoiceAnchors(now: number): void {
  for (const [captureId, anchor] of browserVoiceAnchors) {
    if (anchor.expiresAt <= now) removeBrowserVoiceAnchor(captureId);
  }
}

export function registerBrowserVoiceAnchor(
  captureId: string,
  scope: { pluginId: string; actionId: string; sessionId: string },
  anchor: BrowserVoiceAnchor,
): void {
  const now = Date.now();
  sweepExpiredBrowserVoiceAnchors(now);
  removeBrowserVoiceAnchor(captureId);
  while (browserVoiceAnchors.size >= MAX_BROWSER_VOICE_ANCHORS) {
    const oldest = browserVoiceAnchors.keys().next().value as string | undefined;
    if (!oldest) break;
    removeBrowserVoiceAnchor(oldest);
  }
  const scoped: ScopedBrowserVoiceAnchor = {
    ...scope,
    ...anchor,
    expiresAt: now + BROWSER_VOICE_ANCHOR_TTL_MS,
    expiryTimer: setTimeout(() => {
      if (browserVoiceAnchors.get(captureId) === scoped) browserVoiceAnchors.delete(captureId);
    }, BROWSER_VOICE_ANCHOR_TTL_MS),
  };
  browserVoiceAnchors.set(captureId, scoped);
}

export function removeBrowserVoiceAnchor(captureId: string): void {
  const anchor = browserVoiceAnchors.get(captureId);
  if (anchor) clearTimeout(anchor.expiryTimer);
  browserVoiceAnchors.delete(captureId);
}

export function consumeBrowserVoiceAnchor(
  captureId: string,
  scope: { pluginId: string; actionId: string; sessionId: string },
): BrowserVoiceAnchor | null {
  const anchor = browserVoiceAnchors.get(captureId);
  if (!anchor) return null;
  if (anchor.expiresAt <= Date.now()) {
    removeBrowserVoiceAnchor(captureId);
    return null;
  }
  if (
    anchor.pluginId !== scope.pluginId ||
    anchor.actionId !== scope.actionId ||
    anchor.sessionId !== scope.sessionId
  ) {
    return null;
  }
  removeBrowserVoiceAnchor(captureId);
  return {
    expectedText: anchor.expectedText,
    selectionStart: anchor.selectionStart,
    selectionEnd: anchor.selectionEnd,
  };
}

export function composerDraftOperation(entry: PluginUiEntry): { id: string; operation: ComposerDraftOperation } | null {
  const raw = entry.payload.draft_operation;
  if (!isObject(raw)) return null;
  const id = str(raw, "id");
  const text = str(raw, "text");
  const kind = str(raw, "kind");
  if (!id || text === undefined) return null;
  if (kind === "insert-text" || kind === "set-text") {
    return { id, operation: { kind, text } };
  }
  if (kind === "replace-selection") {
    const captureId = str(raw, "capture_id");
    if ("capture_id" in raw && !captureId) return null;
    return { id, operation: captureId ? { kind, text, captureId } : { kind, text } };
  }
  return null;
}
