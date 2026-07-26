import net from 'node:net';
import tls from 'node:tls';

function normalizeHost(host) {
  return String(host ?? '127.0.0.1').replace(/^\[|\]$/g, '').toLowerCase();
}

export function isLoopbackHost(host) {
  const normalized = normalizeHost(host);
  if (normalized === 'localhost' || normalized === '::1') return true;
  if (net.isIPv4(normalized)) return normalized.startsWith('127.');
  return false;
}

export function assertLoopbackURL(input) {
  const raw = input instanceof URL ? input.href : input?.url ?? String(input);
  const url = new URL(raw);
  if (!['http:', 'https:', 'ws:', 'wss:'].includes(url.protocol) || !isLoopbackHost(url.hostname)) {
    throw new Error('conformance egress guard rejected a non-loopback destination');
  }
  return url;
}

function hostFromConnectArgs(args) {
  if (args[0] && typeof args[0] === 'object') return args[0].host ?? args[0].hostname;
  if (typeof args[1] === 'string') return args[1];
  return undefined;
}

export function installEgressGuard() {
  if (globalThis.__gptLiveEgressGuardInstalled) return;
  globalThis.__gptLiveEgressGuardInstalled = true;

  const originalSocketConnect = net.Socket.prototype.connect;
  net.Socket.prototype.connect = function guardedSocketConnect(...args) {
    const host = hostFromConnectArgs(args);
    if (host !== undefined && !isLoopbackHost(host)) {
      throw new Error('conformance egress guard rejected a non-loopback TCP destination');
    }
    return originalSocketConnect.apply(this, args);
  };

  const originalTlsConnect = tls.connect;
  tls.connect = function guardedTlsConnect(...args) {
    const host = hostFromConnectArgs(args);
    if (host !== undefined && !isLoopbackHost(host)) {
      throw new Error('conformance egress guard rejected a non-loopback TLS destination');
    }
    return originalTlsConnect.apply(this, args);
  };

  const originalFetch = globalThis.fetch;
  globalThis.fetch = function guardedFetch(input, init) {
    assertLoopbackURL(input);
    return originalFetch(input, init);
  };

  if (globalThis.WebSocket) {
    const NativeWebSocket = globalThis.WebSocket;
    globalThis.WebSocket = new Proxy(NativeWebSocket, {
      construct(target, args, newTarget) {
        assertLoopbackURL(args[0]);
        return Reflect.construct(target, args, newTarget);
      },
    });
  }
}
