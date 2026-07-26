#!/usr/bin/env node

import { createHash } from "node:crypto";
import { cp, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const timeoutMs = 180_000;

const mutants = [
  {
    id: "route",
    file: "src/realtime/path.rs",
    from: '"/v1/realtime/client_secrets" => return Ok(RestOperation::CreateClientSecret),',
    to: '"/v1/realtime/client_secretz" => return Ok(RestOperation::CreateClientSecret),',
    test: [
      "test",
      "--locked",
      "--lib",
      "realtime::path::tests::all_ten_rest_operations_have_one_exact_path",
      "--",
      "--exact",
    ],
  },
  {
    id: "capability",
    file: "src/realtime/capability.rs",
    from: `ProfileKind::ChatGpt => Support::Unsupported {
                required_profiles: API_KEY_PROFILES,
            },`,
    to: "ProfileKind::ChatGpt => Support::Native,",
    test: [
      "test",
      "--locked",
      "--lib",
      "realtime::capability::tests::table_and_support_match_independent_exact_oracle",
      "--",
      "--exact",
    ],
  },
  {
    id: "credential-policy",
    file: "src/realtime/contract.rs",
    from: "UpstreamCredentialMode::Client => CredentialPolicy::ClientBearer,",
    to: "UpstreamCredentialMode::Client => CredentialPolicy::Managed,",
    test: [
      "test",
      "--locked",
      "--lib",
      "realtime::contract::tests::call_create_truth_table_covers_content_dialect_and_credentials",
      "--",
      "--exact",
    ],
  },
  {
    id: "header-allowlist",
    file: "src/realtime/headers.rs",
    from: `pub const RESPONSE_HEADER_ALLOWLIST: [&str; 6] = [
    "content-type",
    "location",
    "retry-after",
    "x-request-id",`,
    to: `pub const RESPONSE_HEADER_ALLOWLIST: [&str; 6] = [
    "content-type",
    "location",
    "retry-after",
    "x-request-mutation",`,
    test: [
      "test",
      "--locked",
      "--lib",
      "realtime::headers::tests::response_map_is_exact_and_keeps_all_values_in_upstream_order",
      "--",
      "--exact",
    ],
  },
  {
    id: "avas",
    file: "src/wire/mod.rs",
    from: 'pub const AVAS_QUERY: &str = "intent=quicksilver&architecture=avas";',
    to: 'pub const AVAS_QUERY: &str = "intent=quicksilver&architecture=avaz";',
    test: [
      "test",
      "--locked",
      "--lib",
      "wire::tests::constants_are_verbatim",
      "--",
      "--exact",
    ],
  },
  {
    id: "cap-comparison",
    file: "src/relay/body.rs",
    from: "if buffer.len().saturating_add(chunk.len()) > max_bytes {",
    to: "if buffer.len().saturating_add(chunk.len()) >= max_bytes {",
    test: [
      "test",
      "--locked",
      "--lib",
      "relay::body::tests::the_cap_is_exact",
      "--",
      "--exact",
    ],
  },
  {
    id: "pump-outcome",
    file: "src/relay/pump.rs",
    from: 'Self::Aborted { .. } => "aborted",',
    to: 'Self::Aborted { .. } => "abort_mutation",',
    test: [
      "test",
      "--locked",
      "--test",
      "properties",
      "pump_outcome_labels_are_stable",
      "--",
      "--exact",
    ],
  },
];

function fail(message) {
  throw new Error(`mutation check failed: ${message}`);
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function countOccurrences(source, anchor) {
  if (anchor.length === 0) return 0;
  let count = 0;
  let offset = 0;
  while ((offset = source.indexOf(anchor, offset)) !== -1) {
    count += 1;
    offset += anchor.length;
  }
  return count;
}

function shouldCopy(source) {
  const rel = relative(root, source);
  if (rel === "") return true;
  const excluded = new Set([".git", ".codexclaw", "target", "node_modules"]);
  return !rel.split(/[/\\]/).some((segment) => excluded.has(segment));
}

async function main() {
  if (new Set(mutants.map((mutant) => mutant.id)).size !== mutants.length || mutants.length !== 7) {
    fail("runner must define exactly seven uniquely named mutants");
  }

  const sourceFiles = [...new Set(mutants.map((mutant) => mutant.file))];
  const before = new Map();
  for (const file of sourceFiles) {
    before.set(file, sha256(await readFile(resolve(root, file))));
  }

  const scratch = await mkdtemp(join(tmpdir(), "gpt-live-proxy-mutations-"));
  const sharedTarget = join(scratch, "target");
  let killed = 0;

  try {
    for (const mutant of mutants) {
      const copyRoot = join(scratch, mutant.id);
      await cp(root, copyRoot, {
        recursive: true,
        dereference: false,
        filter: shouldCopy,
      });

      const target = resolve(copyRoot, mutant.file);
      const source = await readFile(target, "utf8");
      const occurrences = countOccurrences(source, mutant.from);
      if (occurrences !== 1) {
        fail(`${mutant.id} source anchor matched ${occurrences} times in ${mutant.file}`);
      }
      if (source.includes(mutant.to)) {
        fail(`${mutant.id} replacement already exists in ${mutant.file}`);
      }
      await writeFile(target, source.replace(mutant.from, mutant.to), "utf8");

      const result = spawnSync("cargo", mutant.test, {
        cwd: copyRoot,
        env: { ...process.env, CARGO_TARGET_DIR: sharedTarget },
        encoding: "utf8",
        timeout: timeoutMs,
        maxBuffer: 8 * 1024 * 1024,
      });
      if (result.error) {
        fail(`${mutant.id} test process error: ${result.error.code ?? result.error.message}`);
      }
      if (result.status === 0) {
        fail(`${mutant.id} survived focused test ${mutant.test.join(" ")}`);
      }
      const combined = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
      if (!combined.includes("test result: FAILED")) {
        fail(`${mutant.id} did not reach an assertion failure (exit=${result.status})`);
      }
      killed += 1;
      process.stdout.write(`killed ${mutant.id}: ${basename(mutant.file)}\n`);
      await rm(copyRoot, { recursive: true, force: true });
    }
  } finally {
    await rm(scratch, { recursive: true, force: true });
    for (const file of sourceFiles) {
      const after = sha256(await readFile(resolve(root, file)));
      if (after !== before.get(file)) {
        fail(`original source changed: ${file}`);
      }
    }
  }

  if (killed !== mutants.length) fail(`killed ${killed}/${mutants.length} mutants`);
  process.stdout.write(`mutation check passed: killed=${killed} survived=0 source_changed=0\n`);
}

await main();
