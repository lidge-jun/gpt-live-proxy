import { spawn } from 'node:child_process';
import { access, readFile } from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { after, before } from 'node:test';

import { assertLoopbackURL, installEgressGuard } from './egress-guard.mjs';

installEgressGuard();

const { startMockUpstream } = await import('./mock-upstream.mjs');
const { registerSdkRestTests } = await import('./sdk-rest.test.mjs');
const { registerSdkWebSocketTests } = await import('./sdk-websocket.test.mjs');
const { registerDocumentedTransportTests } = await import('./documented-transport.test.mjs');

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
let harness;

async function startTlsGateway(targetPort) {
  const keyPath = process.env.GPT_LIVE_CONFORMANCE_TLS_KEY;
  const certPath = process.env.GPT_LIVE_CONFORMANCE_TLS_CERT;
  if (!keyPath || !certPath) throw new Error('launch.mjs must provide loopback TLS material');
  const server = https.createServer({ key: await readFile(keyPath), cert: await readFile(certPath) }, (request, response) => {
    const headers = { ...request.headers, host: `127.0.0.1:${targetPort}` };
    const upstream = http.request({ host: '127.0.0.1', port: targetPort, method: request.method, path: request.url, headers }, (upstreamResponse) => {
      response.writeHead(upstreamResponse.statusCode, upstreamResponse.headers);
      upstreamResponse.pipe(response);
    });
    upstream.on('error', () => response.destroy());
    request.pipe(upstream);
  });
  server.on('upgrade', (request, socket, head) => {
    const upstream = net.connect({ host: '127.0.0.1', port: targetPort }, () => {
      const lines = [`${request.method} ${request.url} HTTP/${request.httpVersion}`];
      for (const [name, value] of Object.entries(request.headers)) {
        if (name.toLowerCase() === 'host') continue;
        if (Array.isArray(value)) for (const item of value) lines.push(`${name}: ${item}`);
        else if (value !== undefined) lines.push(`${name}: ${value}`);
      }
      lines.push(`host: 127.0.0.1:${targetPort}`);
      upstream.write(`${lines.join('\r\n')}\r\n\r\n`);
      if (head.byteLength) upstream.write(head);
      socket.pipe(upstream).pipe(socket);
    });
    upstream.on('error', () => socket.destroy());
  });
  server.listen(0, '127.0.0.1');
  await new Promise((resolve) => server.once('listening', resolve));
  const address = server.address();
  return {
    port: address.port,
    async close() {
      await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
    },
  };
}

async function reservePort() {
  const server = http.createServer();
  await new Promise((resolve, reject) => server.listen(0, '127.0.0.1', (error) => error ? reject(error) : resolve()));
  const address = server.address();
  const port = address.port;
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  return port;
}

async function waitUntilReady(url, child, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`proxy exited before readiness (code ${child.exitCode})`);
    try {
      const response = await fetch(url);
      if (response.status === 200) return;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error('proxy readiness deadline exceeded');
}

async function stopChild(child) {
  if (!child || child.exitCode !== null) return;
  const exited = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGTERM');
  const timer = setTimeout(() => child.kill('SIGKILL'), 5_000);
  await exited;
  clearTimeout(timer);
}

before(async () => {
  const upstream = await startMockUpstream();
  assertLoopbackURL(upstream.baseURL);
  const port = await reservePort();
  const proxyBaseURL = `http://127.0.0.1:${port}/v1`;
  const binary = process.env.GPT_LIVE_PROXY_BIN || path.join(repoRoot, 'target', 'debug', process.platform === 'win32' ? 'gpt-live-proxy.exe' : 'gpt-live-proxy');
  await access(binary);
  const child = spawn(binary, [], {
    cwd: repoRoot,
    stdio: 'ignore',
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      TMPDIR: process.env.TMPDIR,
      NO_PROXY: '127.0.0.1,localhost,::1',
      GPT_LIVE_BIND: `127.0.0.1:${port}`,
      GPT_LIVE_UPSTREAM_MODE: 'apikey',
      GPT_LIVE_CREDENTIAL_MODE: 'client',
      GPT_LIVE_BASE_URL: upstream.baseURL,
      GPT_LIVE_LOG: 'warn',
    },
  });
  try {
    await waitUntilReady(`http://127.0.0.1:${port}/readyz`, child);
  } catch (error) {
    await stopChild(child);
    await upstream.close();
    throw error;
  }
  const gateway = await startTlsGateway(port);
  const secureBaseURL = `https://127.0.0.1:${gateway.port}/v1`;
  harness = {
    child,
    gateway,
    upstream,
    capture: upstream.capture,
    proxyBaseURL: secureBaseURL,
    proxyWebSocketURL: `wss://127.0.0.1:${gateway.port}/v1`,
    apiKey: 'sk_sdk_conformance_key',
    ephemeralKey: 'ek_browser_conformance_key',
    translationKey: 'ek_translation_conformance_key',
  };
});

after(async () => {
  if (!harness) return;
  await harness.gateway.close();
  await stopChild(harness.child);
  await harness.upstream.close();
});

const getHarness = () => harness;
registerSdkRestTests(getHarness);
registerSdkWebSocketTests(getHarness);
registerDocumentedTransportTests(getHarness);
