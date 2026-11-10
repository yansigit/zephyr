// The web app's service worker (DESIGN §4.3.3): shows pushes, and carries a notification's
// button to the daemon. It caches nothing: the app is not for offline use.
"use strict";

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  let push;
  try {
    push = event.data.json();
  } catch (_) {
    return;
  }
  if (push.kind === "card") {
    // A lock-screen button approves only what any owner channel may; the daemon checks again.
    const actions = push.quick ? [{ action: "ok", title: "Approve" }] : [];
    actions.push({ action: "no", title: "Deny" });
    event.waitUntil(
      self.registration.showNotification(String(push.title || "zephyr"), {
        body: String(push.body || ""),
        tag: "card-" + push.id,
        data: { id: push.id },
        icon: "/icon-192.png",
        requireInteraction: true,
        actions,
      })
    );
  } else if (push.kind === "reply") {
    event.waitUntil(
      self.registration.showNotification("zephyr", {
        body: String(push.body || ""),
        tag: "reply",
        icon: "/icon-192.png",
      })
    );
  }
});

function decide(id, verb) {
  return fetch("/api/push/decide", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "same-origin",
    body: JSON.stringify({ id, verb }),
  });
}

// The open app, told which card to show; else a new window on it.
async function show(id) {
  const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  for (const client of windows) {
    if ("focus" in client) {
      client.postMessage({ held: id });
      return client.focus();
    }
  }
  return self.clients.openWindow(id ? "/#held/" + id : "/");
}

self.addEventListener("notificationclick", (event) => {
  const id = event.notification.data && event.notification.data.id;
  event.notification.close();
  if (id && (event.action === "ok" || event.action === "no")) {
    // Refused (a payment, a loosening, a card that changed): the app and its passkey decide.
    const done = decide(id, event.action)
      .then((res) => (res.ok ? null : show(id)))
      .catch(() => show(id));
    event.waitUntil(done);
    return;
  }
  event.waitUntil(show(id));
});

// Swiping a card away denies it, on Android only: elsewhere a notification is also "closed"
// when the system clears it, which must not decide anything.
self.addEventListener("notificationclose", (event) => {
  const id = event.notification.data && event.notification.data.id;
  if (id && self.navigator.userAgent.includes("Android")) {
    event.waitUntil(decide(id, "no").catch(() => null));
  }
});
