// Minimal service worker: enables PWA installability but does not precache.
// The previous version precached `/static/*` paths that no longer exist
// (the app is Vite-built with hashed `/assets/*` files), which generated
// a burst of auth-failing 404s on install and contributed to rate-limit
// lockouts for mobile PWA users.

self.addEventListener("install", () => {
  self.skipWaiting();
});

self.addEventListener("activate", (e) => {
  // Clear any cache from the old precache-all strategy.
  e.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

// No fetch handler: requests go to the network directly. The Vite build
// output is content-hashed, so HTTP caching headers handle offline/cache
// behavior without us re-implementing cache-first logic.

// Web Push receiver. The server POSTs an AES-128-GCM encrypted payload
// to the browser's push endpoint; the browser decrypts it and fires
// this event with the plaintext JSON. Shape:
//   { title, body, url, tag, session_id }
// renotify:true on showNotification is required for iOS to re-buzz the
// lock screen when a notification with a matching tag is already present.
//
// Focused-client suppression: when a PWA window is visible and focused, the
// payload goes to it as an in-app toast instead of an OS notification. Not on
// Apple's push service: WebKit revokes the subscription after a few pushes that
// show no notification, so those always show one. The server likewise never
// sends silent "clear" pushes to Apple endpoints.
// Per-tag high-water mark of the latest seq we've shown or cleared. Lets an
// out-of-order older notify (delivered AFTER a newer clear) be dropped instead
// of resurrecting a handled request's notification: getNotifications only sees
// what is open right now, so a clear cannot close a notify that has not arrived
// yet. See #2491.
// ponytail: in-memory only, lost if the browser terminates the worker between
// pushes; reordering only matters when pushes are near-simultaneous (the worker
// stays warm), so IndexedDB persistence would buy a moot window.
const seqHighWater = new Map();
function isStaleSeq(tag, seq) {
  if (seq == null || !tag) return false;
  const hw = seqHighWater.get(tag);
  return hw != null && seq < hw;
}
function recordSeq(tag, seq) {
  if (seq == null || !tag) return;
  const hw = seqHighWater.get(tag);
  if (hw == null || seq > hw) seqHighWater.set(tag, seq);
}

// Unknown counts as Apple: a needless OS notification beats a revoked subscription.
async function mustShowNotification() {
  try {
    const sub = await self.registration.pushManager.getSubscription();
    if (!sub) return true;
    const host = new URL(sub.endpoint).hostname;
    return host === "push.apple.com" || host.endsWith(".push.apple.com");
  } catch {
    return true;
  }
}

self.addEventListener("push", (event) => {
  let payload = {};
  if (event.data) {
    try {
      payload = event.data.json();
    } catch {
      payload = { title: "Agent of Empires", body: event.data.text() };
    }
  }
  // Retract path: a "clear" push closes a previously shown approval or
  // question notification once that request was handled on another device,
  // so a backgrounded phone or second computer stops showing a stale alert.
  // It shows nothing (the request is gone) and is NOT focus-gated. The seq
  // guard skips closing a NEWER notification when a clear for an earlier
  // request in the same session is delivered out of order. See #2491.
  if (payload.kind === "clear") {
    const tag = payload.tag;
    // Record the clear's seq even when getNotifications is unsupported, so a
    // later older notify for this tag is dropped rather than shown.
    recordSeq(tag, payload.seq);
    event.waitUntil(
      (async () => {
        if (!tag || !self.registration.getNotifications) return;
        try {
          const existing = await self.registration.getNotifications({ tag });
          for (const n of existing) {
            const nseq = n.data && n.data.seq;
            if (nseq == null || payload.seq == null || nseq <= payload.seq) {
              n.close();
            }
          }
        } catch {
          /* getNotifications may be unsupported or throw; never fail the push */
        }
      })(),
    );
    return;
  }

  // Drop an out-of-order notify that is older than a clear (or newer notify)
  // already seen for this tag: the request it announces was already handled.
  const tag = payload.tag || "aoe";
  if (isStaleSeq(tag, payload.seq)) {
    // Apple revokes after silent pushes, so there the stale notice is shown under its own tag, keeping the newer
    // one, and closed at once.
    event.waitUntil(
      (async () => {
        if (!(await mustShowNotification())) return;
        const staleTag = `${tag}:stale`;
        await self.registration.showNotification(payload.title || "Agent of Empires", { tag: staleTag, silent: true });
        try {
          for (const n of await self.registration.getNotifications({ tag: staleTag })) n.close();
        } catch {
          /* getNotifications may be unsupported; the notice then stays until dismissed */
        }
      })(),
    );
    return;
  }
  recordSeq(tag, payload.seq);

  const title = payload.title || "Agent of Empires";
  const options = {
    body: payload.body || "",
    tag,
    renotify: true,
    // Store tag + seq so a later "clear" push can match this notification
    // and the seq guard can avoid closing it if a newer one supersedes it.
    data: { url: payload.url || "/", tag: payload.tag, seq: payload.seq },
    icon: "/icon-192.png",
    badge: "/icon-192.png",
  };

  event.waitUntil(
    (async () => {
      const clientList = await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      const focused = clientList.find((c) => c.visibilityState === "visible" && c.focused);
      if (focused && !(await mustShowNotification())) {
        try {
          focused.postMessage({ type: "aoe-push", payload });
        } catch {
          /* ignore */
        }
        return;
      }
      await self.registration.showNotification(title, options);
    })(),
  );
});

// Tap-to-open. Look for an existing PWA window first so we focus it
// (and navigate if needed) rather than opening a second instance.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = (event.notification.data && event.notification.data.url) || "/";
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then(async (clientList) => {
      for (const client of clientList) {
        if ("focus" in client) {
          if (client.url !== target && "navigate" in client) {
            try {
              await client.navigate(target);
            } catch {
              /* SW may not be able to navigate across origins etc; ignore */
            }
          }
          return client.focus();
        }
      }
      if (self.clients.openWindow) {
        return self.clients.openWindow(target);
      }
    }),
  );
});
