/* CalStack service worker: web push display + click-through. */
self.addEventListener('push', (event) => {
  let data = {};
  try { data = event.data ? event.data.json() : {}; } catch (e) { /* payload optional */ }
  event.waitUntil(
    self.registration.showNotification(data.title || 'CalStack', {
      body: data.body || '',
      data: { url: data.url || '/' },
    })
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil(clients.openWindow(event.notification.data.url || '/'));
});