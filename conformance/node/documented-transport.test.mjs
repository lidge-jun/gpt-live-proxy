import assert from 'node:assert/strict';
import { test } from 'node:test';
import OpenAI from 'openai';
import { OpenAIRealtimeWS } from 'openai/realtime/ws';
import WebSocket from 'ws';

import { assertCallID, closeRealtime, futureEvent, openSocket, onceSocket } from './test-helpers.mjs';

async function triggerRawSocket(socket) {
  await openSocket(socket);
  const message = onceSocket(socket, 'message');
  socket.send(JSON.stringify({ type: 'session.update', session: { type: 'translation' } }));
  await message;
  const closed = onceSocket(socket, 'close');
  socket.close(1000, 'conformance complete');
  await closed;
}

export function registerDocumentedTransportTests(getHarness) {
  test('documented multipart and raw-SDP WebRTC compose Location into SDK sideband', async () => {
    const harness = getHarness();
    const form = new FormData();
    form.set('sdp', 'v=0\r\na=offer-multipart');
    form.set('session', JSON.stringify({ type: 'realtime', model: 'gpt-realtime-2.1' }));
    const multipart = await fetch(`${harness.proxyBaseURL}/realtime/calls`, {
      method: 'POST', headers: { authorization: `Bearer ${harness.apiKey}` }, body: form,
    });
    assert.equal(multipart.status, 201);
    const callID = assertCallID(multipart.headers.get('location'));
    assert.equal(await multipart.text(), 'v=0\r\na=mock-answer');

    const raw = await fetch(`${harness.proxyBaseURL}/realtime/calls`, {
      method: 'POST',
      headers: { authorization: `Bearer ${harness.ephemeralKey}`, 'content-type': 'application/sdp' },
      body: 'v=0\r\na=offer-raw',
    });
    assert.equal(raw.status, 201);
    assertCallID(raw.headers.get('location'));

    const client = new OpenAI({ apiKey: harness.apiKey, baseURL: harness.proxyBaseURL });
    const sideband = new OpenAIRealtimeWS({ callID }, client);
    sideband.on('error', () => {});
    await openSocket(sideband.socket);
    const unknown = futureEvent(sideband);
    sideband.send({ type: 'session.update', session: { type: 'realtime' } });
    assert.equal((await unknown).type, 'server.future_event');
    await closeRealtime(sideband);
  });

  test('documented translation REST and WebSocket paths relay unchanged', async () => {
    const harness = getHarness();
    const secret = await fetch(`${harness.proxyBaseURL}/realtime/translations/client_secrets`, {
      method: 'POST', headers: { authorization: `Bearer ${harness.apiKey}`, 'content-type': 'application/json' }, body: JSON.stringify({ session: { type: 'translation' } }),
    });
    assert.equal(secret.status, 200);
    assert.equal((await secret.json()).session.type, 'translation');
    const call = await fetch(`${harness.proxyBaseURL}/realtime/translations/calls`, {
      method: 'POST', headers: { authorization: `Bearer ${harness.translationKey}`, 'content-type': 'application/sdp' }, body: 'v=0\r\na=translation',
    });
    assert.equal(call.status, 201);
    await triggerRawSocket(new WebSocket(
      `${harness.proxyWebSocketURL}/realtime/translations?model=gpt-realtime-translate`,
      { headers: { authorization: `Bearer ${harness.apiKey}` } },
    ));
  });

  test('documented optional organization and project browser protocols survive the proxy', async () => {
    const harness = getHarness();
    const checkpoint = harness.capture.checkpoint();
    const socket = new WebSocket(`${harness.proxyWebSocketURL}/realtime?model=gpt-realtime-2.1`, [
      'realtime', `openai-insecure-api-key.${harness.ephemeralKey}`, 'openai-organization.org_conformance', 'openai-project.proj_conformance',
    ]);
    await triggerRawSocket(socket);
    assert.equal(socket.protocol, 'realtime');
    const upgrade = harness.capture.since(checkpoint).find((row) => row.kind === 'websocket.upgrade');
    assert.deepEqual(upgrade.protocolClasses, ['realtime', 'ephemeral-api-key', 'organization', 'project']);
    assert.ok(!upgrade.headerNames.includes('authorization'));
  });
}
