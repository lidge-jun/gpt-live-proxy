import assert from 'node:assert/strict';
import { test } from 'node:test';
import OpenAI from 'openai';

import { sha256 } from './wire-capture.mjs';

export function registerSdkRestTests(getHarness) {
  test('official SDK REST helpers use only baseURL and preserve all five routes', async () => {
    const harness = getHarness();
    const checkpoint = harness.capture.checkpoint();
    const client = new OpenAI({ apiKey: harness.apiKey, baseURL: harness.proxyBaseURL });

    const secret = await client.realtime.clientSecrets.create({ session: { type: 'realtime', model: 'gpt-realtime-2.1' } });
    assert.equal(secret.session.type, 'realtime');
    assert.equal(typeof secret.value, 'string');
    await client.realtime.calls.accept('rtc_sdk_accept', { type: 'realtime' });
    await client.realtime.calls.reject('rtc_sdk_reject', { status_code: 486 });
    await client.realtime.calls.refer('rtc_sdk_refer', { target_uri: 'sip:agent@example.test' });
    await client.realtime.calls.hangup('rtc_sdk_hangup');

    const requests = harness.capture.since(checkpoint).filter((row) => row.kind === 'http.request');
    const expected = [
      ['/v1/realtime/client_secrets', { session: { type: 'realtime', model: 'gpt-realtime-2.1' } }],
      ['/v1/realtime/calls/rtc_sdk_accept/accept', { type: 'realtime' }],
      ['/v1/realtime/calls/rtc_sdk_reject/reject', { status_code: 486 }],
      ['/v1/realtime/calls/rtc_sdk_refer/refer', { target_uri: 'sip:agent@example.test' }],
      ['/v1/realtime/calls/rtc_sdk_hangup/hangup', null],
    ];
    assert.deepEqual(requests.map((row) => row.path), expected.map(([path]) => path));
    const authHash = sha256(`Bearer ${harness.apiKey}`);
    for (const [index, row] of requests.entries()) {
      const body = expected[index][1];
      const bytes = body === null ? Buffer.alloc(0) : Buffer.from(JSON.stringify(body));
      assert.equal(row.method, 'POST');
      assert.equal(row.authorizationSha256, authHash);
      assert.deepEqual(
        row.contractHeaderNames,
        body === null ? ['accept', 'authorization'] : ['accept', 'authorization', 'content-type'],
      );
      assert.equal(row.bodyLength, bytes.byteLength);
      assert.equal(row.bodySha256, sha256(bytes));
    }
  });
}
