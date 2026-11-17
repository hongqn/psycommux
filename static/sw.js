// psycommux service worker: shows pushed notifications while the app is
// closed, and opens the app at the notification's workspace when tapped.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  let m = {};
  try { m = event.data ? event.data.json() : {}; } catch (e) { m = { body: event.data ? event.data.text() : "" }; }
  event.waitUntil(self.registration.showNotification(m.title || "psycommux", {
    body: m.body || "",
    tag: m.tag || undefined,
    icon: "/icon-192.png",
    data: { url: m.url || "/" },
  }));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const url = new URL((event.notification.data && event.notification.data.url) || "/", self.location.origin).href;
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const w of windows) {
      await w.focus();
      w.postMessage({ type: "open", url });
      return;
    }
    await self.clients.openWindow(url);
  })());
});
