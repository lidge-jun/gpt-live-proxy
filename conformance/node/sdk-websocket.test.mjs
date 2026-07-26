import assert from 'node:assert/strict';
import { test } from 'node:test';
import OpenAI from 'openai';
import { OpenAIRealtimeWS } from 'openai/realtime/ws';
import { OpenAIRealtimeWebSocket } from 'openai/realtime/websocket';
import StandardsWebSocket from 'ws';

import { closeRealtime, futureEvent, openSocket } from './test-helpers.mjs';
import { sha256 } from './wire-capture.mjs';

async function exercise(realtime) {
  realtime.on('error', () => {});
  await openSocket(realtime.socket);
  const session = realtime.emitted('session.created');
  const audio = realtime.emitted('response.output_audio.delta');
  const tool = realtime.emitted('response.function_call_arguments.delta');
  const response = realtime.emitted('response.done');
  const unknown = futureEvent(realtime);
  const typedError = realtime.emitted('error');
  realtime.send({ type: 'session.update', session: { type: 'realtime' } });
  const [sessionEvent, audioEvent, toolEvent, responseEvent, unknownEvent, error] =
    await Promise.all([session, audio, tool, response, unknown, typedError]);
  assert.equal(sessionEvent.type, 'session.created');
  assert.equal(audioEvent.type, 'response.output_audio.delta');
  assert.equal(toolEvent.type, 'response.function_call_arguments.delta');
  assert.equal(responseEvent.type, 'response.done');
  assert.equal(unknownEvent.type, 'server.future_event');
  assert.equal(error.error.code, 'mock_error');
  realtime.send({ type: 'input_audio_buffer.append', audio: 'AA==' });
  realtime.send({ type: 'response.create', response: { output_modalities: ['text'] } });
  await closeRealtime(realtime);
}

export function registerSdkWebSocketTests(getHarness) {
  test('official server SDK standalone and existing-call WebSockets relay typed and unknown events', async () => {
    const harness = getHarness();
    const checkpoint = harness.capture.checkpoint();
    const client = new OpenAI({ apiKey: harness.apiKey, baseURL: harness.proxyBaseURL });
    await exercise(new OpenAIRealtimeWS({ model: 'gpt-realtime-2.1' }, client));
    await exercise(new OpenAIRealtimeWS({ callID: 'rtc_sdk_sideband' }, client));
    const upgrades = harness.capture.since(checkpoint).filter((row) => row.kind === 'websocket.upgrade');
    assert.deepEqual(upgrades.map((row) => row.path), [
      '/v1/realtime?model=gpt-realtime-2.1',
      '/v1/realtime?call_id=rtc_sdk_sideband',
    ]);
    const authHash = sha256(`Bearer ${harness.apiKey}`);
    for (const upgrade of upgrades) {
      assert.equal(upgrade.method, 'GET');
      assert.deepEqual(upgrade.contractHeaderNames, ['authorization']);
      assert.equal(upgrade.authorizationSha256, authHash);
      assert.deepEqual(upgrade.protocolClasses, []);
      assert.deepEqual(upgrade.protocolAuthSha256s, []);
    }
  });

  test('official browser SDK uses realtime and ephemeral-key protocols', async () => {
    const harness = getHarness();
    const checkpoint = harness.capture.checkpoint();
    const client = new OpenAI({ apiKey: harness.ephemeralKey, baseURL: harness.proxyBaseURL, dangerouslyAllowBrowser: true });
    const nativeWebSocket = globalThis.WebSocket;
    globalThis.WebSocket = StandardsWebSocket;
    let realtime;
    try {
      realtime = new OpenAIRealtimeWebSocket({ model: 'gpt-realtime-2.1', dangerouslyAllowBrowser: true }, client);
      await exercise(realtime);
      assert.equal(realtime.socket.protocol, 'realtime');
    } finally {
      globalThis.WebSocket = nativeWebSocket;
    }
    const upgrade = harness.capture.since(checkpoint).find((row) => row.kind === 'websocket.upgrade');
    assert.equal(upgrade.method, 'GET');
    assert.equal(upgrade.path, '/v1/realtime?model=gpt-realtime-2.1');
    assert.deepEqual(upgrade.protocolClasses, ['realtime', 'ephemeral-api-key']);
    assert.deepEqual(upgrade.contractHeaderNames, ['sec-websocket-protocol']);
    assert.equal(upgrade.authorizationSha256, null);
    assert.deepEqual(upgrade.protocolAuthSha256s, [
      sha256(`openai-insecure-api-key.${harness.ephemeralKey}`),
    ]);
  });
}
