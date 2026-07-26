import http from 'node:http';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';

import { WireCapture, frameMetadata, headerNames, sha256 } from './wire-capture.mjs';

const SERVER_EVENTS = [
  { type: 'session.created', event_id: 'evt_session', session: { id: 'sess_mock', type: 'realtime', object: 'realtime.session' } },
  { type: 'response.output_audio.delta', event_id: 'evt_audio', response_id: 'resp_mock', item_id: 'item_mock', output_index: 0, content_index: 0, delta: 'AA==' },
  { type: 'response.function_call_arguments.delta', event_id: 'evt_tool', response_id: 'resp_mock', item_id: 'item_tool', output_index: 0, call_id: 'call_mock', delta: '{}' },
  { type: 'response.done', event_id: 'evt_response', response: { id: 'resp_mock', object: 'realtime.response', status: 'completed', output: [] } },
  { type: 'server.future_event', event_id: 'evt_unknown', opaque: true },
  { type: 'error', event_id: 'evt_error', error: { type: 'invalid_request_error', code: 'mock_error', message: 'mock typed error', param: null, event_id: 'evt_error' } },
];

const CONTRACT_HEADERS = new Set([
  'accept',
  'authorization',
  'content-type',
  'idempotency-key',
  'openai-alpha',
  'openai-beta',
  'openai-organization',
  'openai-project',
  'openai-safety-identifier',
  'origin',
  'sec-websocket-protocol',
  'x-oai-attestation',
]);

function safeContractHeaders(headers) {
  const names = headerNames(headers).filter((name) => CONTRACT_HEADERS.has(name));
  const authorization = headers.authorization;
  return {
    contractHeaderNames: names,
    authorizationSha256: typeof authorization === 'string' ? sha256(authorization) : null,
  };
}

function protocolClass(token) {
  if (token === 'realtime') return 'realtime';
  if (token.startsWith('openai-insecure-api-key.')) return 'ephemeral-api-key';
  if (token.startsWith('openai-organization.')) return 'organization';
  if (token.startsWith('openai-project.')) return 'project';
  return 'other';
}

async function readBody(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks);
}

function json(response, status, value, extraHeaders = {}) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, {
    'content-type': 'application/json',
    'content-length': String(body.byteLength),
    'x-request-id': 'req_mock_safe',
    ...extraHeaders,
  });
  response.end(body);
}

export async function startMockUpstream() {
  const capture = new WireCapture();
  const sockets = new Set();
  let callSequence = 0;

  const server = http.createServer(async (request, response) => {
    try {
      const body = await readBody(request);
      capture.record({
        kind: 'http.request',
        method: request.method,
        path: request.url,
        headerNames: headerNames(request.headers),
        ...safeContractHeaders(request.headers),
        bodyLength: body.byteLength,
        bodySha256: sha256(body),
      });

      const path = new URL(request.url, 'http://127.0.0.1').pathname;
      if (path === '/v1/realtime/client_secrets') {
        json(response, 200, {
          value: 'ek_mock_ephemeral_value',
          expires_at: 2_000_000_000,
          session: { id: 'sess_mock', object: 'realtime.session', type: 'realtime', model: 'gpt-realtime-2.1' },
        });
        return;
      }
      if (path === '/v1/realtime/translations/client_secrets') {
        json(response, 200, {
          value: 'ek_mock_translation_value',
          expires_at: 2_000_000_000,
          session: { id: 'sess_translation', object: 'realtime.session', type: 'translation', model: 'gpt-realtime-translate' },
        });
        return;
      }
      if (/^\/v1\/realtime\/calls\/[^/]+\/(accept|reject|refer|hangup)$/.test(path)) {
        response.writeHead(204, { 'x-request-id': 'req_mock_safe' });
        response.end();
        return;
      }
      if (path === '/v1/realtime/calls' || path === '/v1/realtime/translations/calls') {
        callSequence += 1;
        const callID = path.includes('/translations/') ? `rtc_translation_${callSequence}` : `rtc_sdk_location_${callSequence}`;
        const answer = Buffer.from('v=0\r\na=mock-answer');
        response.writeHead(201, {
          'content-type': 'application/sdp',
          'content-length': String(answer.byteLength),
          location: `/v1/realtime/calls/${callID}`,
          'x-request-id': 'req_mock_safe',
        });
        response.end(answer);
        return;
      }

      json(response, 404, { error: { code: 'mock_unknown_route' } });
    } catch {
      if (!response.headersSent) json(response, 500, { error: { code: 'mock_failure' } });
      else response.destroy();
    }
  });

  server.on('connection', (socket) => {
    sockets.add(socket);
    socket.once('close', () => sockets.delete(socket));
  });

  const websocketServer = new WebSocketServer({
    noServer: true,
    handleProtocols(protocols) {
      return protocols.has('realtime') ? 'realtime' : undefined;
    },
  });

  server.on('upgrade', (request, socket, head) => {
    const protocolTokens = String(request.headers['sec-websocket-protocol'] ?? '')
      .split(',')
      .map((token) => token.trim())
      .filter(Boolean);
    capture.record({
      kind: 'websocket.upgrade',
      method: request.method,
      path: request.url,
      headerNames: headerNames(request.headers),
      ...safeContractHeaders(request.headers),
      protocolClasses: protocolTokens.map(protocolClass),
      protocolAuthSha256s: protocolTokens
        .filter((token) => token.startsWith('openai-insecure-api-key.'))
        .map((token) => sha256(token)),
    });
    websocketServer.handleUpgrade(request, socket, head, (websocket) => {
      websocketServer.emit('connection', websocket, request);
    });
  });

  websocketServer.on('connection', (websocket) => {
    let eventsSent = false;
    websocket.on('message', (data, isBinary) => {
      capture.record(frameMetadata('client-to-upstream', data, isBinary));
      if (eventsSent) return;
      eventsSent = true;
      for (const event of SERVER_EVENTS) {
        const payload = JSON.stringify(event);
        capture.record(frameMetadata('upstream-to-client', payload, false));
        websocket.send(payload);
      }
    });
    websocket.on('close', (code) => {
      capture.record({ kind: 'websocket.close', code, direction: 'upstream-observed' });
    });
  });

  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('mock upstream did not bind TCP');

  return {
    baseURL: `http://127.0.0.1:${address.port}/v1`,
    capture,
    async close() {
      for (const client of websocketServer.clients) client.terminate();
      websocketServer.close();
      for (const socket of sockets) socket.destroy();
      await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
    },
  };
}
