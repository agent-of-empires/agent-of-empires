import { useState } from "react";

import {
  applyPluginUpdate,
  discoverPlugins,
  dismissPluginUpdate,
  fetchPluginUpdates,
  previewPluginInstall,
  previewPluginUpdate,
  startPluginInstall,
  type PluginDiscoveryResult,
  type PluginInstallConsent,
  type PluginUpdateChangelog,
  type PluginUpdateConsent,
  type PluginUpdateStatus,
  type PluginView,
} from "../../lib/api";
import { reportInfo } from "../../lib/toastBus";

export interface PluginJobRef {
  id: string;
  title: string;
}

interface UpdateReview {
  plugin: PluginView;
  fromVersion: string;
  toVersion: string;
  changelog: PluginUpdateChangelog;
  /** Non-null only when the update expands access. */
  consent: PluginUpdateConsent | null;
  fingerprint: string;
}

/** On-demand update checks and the preview, review and apply flow. */
export function usePluginUpdates(setError: (e: string | null) => void, startJob: (job: PluginJobRef) => void) {
  const [updates, setUpdates] = useState<Record<string, PluginUpdateStatus>>({});
  const [checking, setChecking] = useState(false);
  const [updatingId, setUpdatingId] = useState<string | null>(null);
  const [review, setReview] = useState<UpdateReview | null>(null);
  const [busy, setBusy] = useState(false);
  const [reviewError, setReviewError] = useState<string | null>(null);

  const clearBadge = (id: string) =>
    setUpdates((u) => {
      const next = { ...u };
      delete next[id];
      return next;
    });

  const check = async () => {
    setChecking(true);
    setError(null);
    try {
      const res = await fetchPluginUpdates();
      if (res.kind === "ok") {
        setUpdates(Object.fromEntries(res.updates.map((s) => [s.id, s])));
      } else {
        setUpdates({});
        setError(res.message);
      }
    } finally {
      setChecking(false);
    }
  };

  const preview = async (plugin: PluginView) => {
    setUpdatingId(plugin.id);
    setError(null);
    try {
      const res = await previewPluginUpdate(plugin.id);
      if (res.kind !== "ok") {
        setError(res.message);
        return;
      }
      const p = res.preview;
      setReviewError(null);
      if (p.kind === "no_update") {
        reportInfo(`${plugin.name} is already up to date.`);
        clearBadge(plugin.id);
      } else if (p.kind === "safe_update") {
        setReview({
          plugin,
          fromVersion: plugin.version,
          toVersion: p.to_version,
          changelog: p.changelog,
          consent: null,
          fingerprint: p.fingerprint,
        });
      } else {
        setReview({
          plugin,
          fromVersion: p.consent.from_version,
          toVersion: p.consent.to_version,
          changelog: p.consent.changelog,
          consent: p.consent,
          fingerprint: p.consent.fingerprint,
        });
      }
    } finally {
      setUpdatingId(null);
    }
  };

  const withBusy = async (fn: () => Promise<void>) => {
    setBusy(true);
    setReviewError(null);
    try {
      await fn();
    } finally {
      setBusy(false);
    }
  };

  const approve = () =>
    review &&
    withBusy(async () => {
      const res = await applyPluginUpdate(review.plugin.id, review.fingerprint);
      if (res.kind !== "ok") return setReviewError(res.message);
      clearBadge(review.plugin.id);
      setReview(null);
      startJob({ id: res.jobId, title: `Updating ${review.plugin.name}` });
    });

  // Local state clears only once the backend recorded the dismissal.
  const decline = () =>
    review?.consent &&
    withBusy(async () => {
      const res = await dismissPluginUpdate(review.plugin.id, review.consent!.fingerprint);
      if (res.kind !== "ok") return setReviewError(res.message);
      clearBadge(review.plugin.id);
      setReview(null);
    });

  return {
    updates,
    checking,
    updatingId,
    review,
    busy,
    reviewError,
    check,
    preview,
    approve,
    decline,
    closeReview: () => setReview(null),
  };
}

/** The `gh:owner/repo` source taken from a discovery row's install command. */
export const sourceFromCommand = (command: string) => command.replace(/^.*\binstall\s+/, "").trim();

/** GitHub discovery and the preview, consent and install flow. */
export function usePluginMarketplace(startJob: (job: PluginJobRef) => void) {
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<PluginDiscoveryResult[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [discovering, setDiscovering] = useState(false);
  const [previewingSource, setPreviewingSource] = useState<string | null>(null);
  const [consent, setConsent] = useState<PluginInstallConsent | null>(null);
  const [installBusy, setInstallBusy] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);

  const discover = async () => {
    setDiscovering(true);
    setError(null);
    const res = await discoverPlugins(query);
    if (res.kind === "ok") {
      setResults(res.results);
    } else {
      setResults(null);
      setError(res.message);
    }
    setDiscovering(false);
  };

  const install = async (source: string) => {
    setPreviewingSource(source);
    setError(null);
    setInstallError(null);
    try {
      const res = await previewPluginInstall(source);
      if (res.kind === "ok") setConsent(res.consent);
      else setError(res.message);
    } finally {
      setPreviewingSource(null);
    }
  };

  const approveInstall = async () => {
    if (!consent) return;
    setInstallBusy(true);
    setInstallError(null);
    try {
      const res = await startPluginInstall(consent.source, consent.fingerprint);
      if (res.kind === "ok") {
        setConsent(null);
        startJob({ id: res.jobId, title: `Installing ${consent.id}` });
      } else {
        setInstallError(res.message);
      }
    } finally {
      setInstallBusy(false);
    }
  };

  return {
    query,
    setQuery,
    results,
    error,
    discovering,
    previewingSource,
    consent,
    installBusy,
    installError,
    discover,
    install,
    approveInstall,
    closeConsent: () => setConsent(null),
  };
}

export type PluginMarketplaceState = ReturnType<typeof usePluginMarketplace>;
