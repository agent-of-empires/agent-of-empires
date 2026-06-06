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
// Focused-client suppression: if any PWA window is currently visible
// and focused when the push arrives, we skip the OS notification and
// postMessage the payload to the client so it can show an in-app toast
// instead. userVisibleOnly demands SOMETHING user-visible per push;
// iOS may warn if it stays silent indefinitely, but in practice a
// focused tab is rare enough for pushes that revocation hasn't been
// an issue. If it becomes one, fall back to showing the notification
// anyway and let the client suppress its own toast.
self.addEventListener("push", (event) => {
  let payload = {};
  if (event.data) {
    try {
      payload = event.data.json();
    } catch {
      payload = { title: "Agent of Empires", body: event.data.text() };
    }
  }
  const title = payload.title || "Agent of Empires";
  const options = {
    body: payload.body || "",
    tag: payload.tag || "aoe",
    renotify: true,
    data: { url: payload.url || "/" },
    icon: "/icon-192.png",
    badge: "/icon-192.png",
  };

  event.waitUntil(
    (async () => {
      const clientList = await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      const focused = clientList.find(
        (c) => c.visibilityState === "visible" && c.focused,
      );
      if (focused) {
        // User is already in the app, forward the payload for an in-app
        // toast, skip the OS notification. If the client has no handler,
        // the message is silently dropped which is fine.
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
  const target =
    (event.notification.data && event.notification.data.url) || "/";
  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then(async (clientList) => {
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
