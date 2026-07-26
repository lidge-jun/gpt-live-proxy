import { execFile, spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

function run(command, args) {
  return new Promise((resolve, reject) => {
    execFile(command, args, { stdio: 'ignore' }, (error) => error ? reject(error) : resolve());
  });
}

const directory = await mkdtemp(path.join(os.tmpdir(), 'gpt-live-proxy-conformance-'));
const keyPath = path.join(directory, 'key.pem');
const certPath = path.join(directory, 'cert.pem');

try {
  await run('openssl', [
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
    '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1',
    '-keyout', keyPath, '-out', certPath,
  ]);
  const child = spawn(process.execPath, ['--test', '--test-concurrency=1', 'runner.mjs'], {
    cwd: import.meta.dirname,
    stdio: 'inherit',
    env: {
      ...process.env,
      NODE_EXTRA_CA_CERTS: certPath,
      GPT_LIVE_CONFORMANCE_TLS_KEY: keyPath,
      GPT_LIVE_CONFORMANCE_TLS_CERT: certPath,
    },
  });
  const code = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (status, signal) => resolve(status ?? (signal ? 1 : 0)));
  });
  process.exitCode = code;
} finally {
  await rm(directory, { recursive: true, force: true });
}
