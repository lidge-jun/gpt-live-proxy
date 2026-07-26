import assert from 'node:assert/strict';

export function onceSocket(socket, event, timeoutMs = 5_000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`timed out waiting for WebSocket ${event}`)), timeoutMs);
    const done = (value) => { clearTimeout(timer); resolve(value); };
    const fail = () => { clearTimeout(timer); reject(new Error('WebSocket failed')); };
    if (typeof socket.once === 'function') {
      socket.once(event, done);
      if (event !== 'error') socket.once('error', fail);
    } else {
      socket.addEventListener(event, done, { once: true });
      if (event !== 'error') socket.addEventListener('error', fail, { once: true });
    }
  });
}

export async function openSocket(socket) {
  if (socket.readyState === 1) return;
  await onceSocket(socket, 'open');
}

export async function closeRealtime(realtime) {
  if (realtime.socket.readyState >= 2) return;
  const closed = onceSocket(realtime.socket, 'close');
  realtime.close({ code: 1000, reason: 'conformance complete' });
  await closed;
}

export function futureEvent(realtime) {
  return new Promise((resolve) => realtime.on('event', (event) => {
    if (event.type === 'server.future_event') resolve(event);
  }));
}

export function assertCallID(location) {
  const match = /^\/v1\/realtime\/calls\/(rtc_[A-Za-z0-9_-]{1,128})$/.exec(location);
  assert.ok(match, 'Location must carry a valid rtc_ call ID');
  return match[1];
}
