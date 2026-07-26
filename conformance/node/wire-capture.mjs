import { createHash } from 'node:crypto';
import { EventEmitter } from 'node:events';

export function sha256(value) {
  return createHash('sha256').update(value).digest('hex');
}

export function headerNames(headers) {
  if (!headers) return [];
  if (typeof headers.keys === 'function') {
    return [...new Set([...headers.keys()].map((name) => name.toLowerCase()))].sort();
  }
  return [...new Set(Object.keys(headers).map((name) => name.toLowerCase()))].sort();
}

export function frameMetadata(direction, value, isBinary = false) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(value);
  return {
    kind: 'websocket.frame',
    direction,
    frameType: isBinary ? 'binary' : 'text',
    byteLength: bytes.byteLength,
    sha256: sha256(bytes),
  };
}

export class WireCapture {
  #records = [];
  #events = new EventEmitter();

  checkpoint() {
    return this.#records.length;
  }

  record(metadata) {
    const record = Object.freeze({ sequence: this.#records.length, ...metadata });
    this.#records.push(record);
    this.#events.emit('record', record);
    return record;
  }

  since(checkpoint = 0) {
    return this.#records.slice(checkpoint);
  }

  async waitFor(predicate, { after = 0, timeoutMs = 5_000 } = {}) {
    const existing = this.#records.slice(after).find(predicate);
    if (existing) return existing;

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#events.off('record', onRecord);
        reject(new Error('timed out waiting for metadata-only wire capture'));
      }, timeoutMs);
      timer.unref?.();

      const onRecord = (record) => {
        if (!predicate(record)) return;
        clearTimeout(timer);
        this.#events.off('record', onRecord);
        resolve(record);
      };
      this.#events.on('record', onRecord);
    });
  }
}
