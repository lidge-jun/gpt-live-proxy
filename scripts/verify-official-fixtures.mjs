#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = resolve(root, "tests/fixtures/official/manifest.json");
const inventoryOrder = [
  "standard_client",
  "standard_server",
  "translation_client",
  "translation_server",
];

function fail(message) {
  throw new Error(`official fixture verification failed: ${message}`);
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function assertHex(value, length, label) {
  if (typeof value !== "string" || !new RegExp(`^[0-9a-f]{${length}}$`).test(value)) {
    fail(`${label} is not a ${length}-digit lowercase hex digest`);
  }
}

function assertExactKeys(value, expected, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    fail(`${label} must be an object`);
  }
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (JSON.stringify(actual) !== JSON.stringify(wanted)) {
    fail(`${label} keys differ: got ${actual.join(",")}`);
  }
}

function canonicalSource(fixture) {
  return Buffer.from(
    inventoryOrder
      .map((name) => `[${name}]\n${fixture[name].join("\n")}\n`)
      .join(""),
    "utf8",
  );
}

async function main() {
  const manifestBytes = await readFile(manifestPath);
  const manifest = JSON.parse(manifestBytes);
  assertExactKeys(manifest, ["schema_version", "retrieved_on", "sdk", "fixtures"], "manifest");
  if (manifest.schema_version !== 1) fail("unsupported schema_version");
  if (!/^\d{4}-\d{2}-\d{2}$/.test(manifest.retrieved_on)) fail("invalid retrieved_on");

  assertExactKeys(
    manifest.sdk,
    [
      "package",
      "version",
      "registry_metadata_url",
      "tarball_url",
      "integrity",
      "shasum",
      "file_count",
      "unpacked_size",
    ],
    "sdk",
  );
  if (manifest.sdk.package !== "openai" || manifest.sdk.version !== "6.49.0") {
    fail("SDK package/version drifted from the pinned conformance authority");
  }
  if (!manifest.sdk.registry_metadata_url.startsWith("https://registry.npmjs.org/")) {
    fail("SDK metadata URL is not the npm registry");
  }
  if (!manifest.sdk.tarball_url.startsWith("https://registry.npmjs.org/")) {
    fail("SDK tarball URL is not the npm registry");
  }
  if (!manifest.sdk.integrity.startsWith("sha512-")) fail("SDK integrity is not SHA-512");
  const integrityBytes = Buffer.from(manifest.sdk.integrity.slice("sha512-".length), "base64");
  if (integrityBytes.length !== 64) fail("SDK integrity payload is not 64 bytes");
  assertHex(manifest.sdk.shasum, 40, "SDK shasum");
  if (!Number.isSafeInteger(manifest.sdk.file_count) || manifest.sdk.file_count <= 0) {
    fail("SDK file_count must be a positive integer");
  }
  if (!Number.isSafeInteger(manifest.sdk.unpacked_size) || manifest.sdk.unpacked_size <= 0) {
    fail("SDK unpacked_size must be a positive integer");
  }

  if (!Array.isArray(manifest.fixtures) || manifest.fixtures.length === 0) {
    fail("fixtures must be a non-empty array");
  }
  const fixturePaths = new Set();
  const sourcePaths = new Set();
  let inventoryCount = 0;
  let eventCount = 0;

  for (const entry of manifest.fixtures) {
    assertExactKeys(
      entry,
      ["path", "sources", "canonical_source", "checked_in_fixture_sha256", "inventories"],
      `fixture entry ${entry.path ?? "<missing>"}`,
    );
    if (typeof entry.path !== "string" || entry.path.startsWith("/") || entry.path.includes("..")) {
      fail("fixture path must be repository-relative without parent traversal");
    }
    if (fixturePaths.has(entry.path)) fail(`duplicate fixture path ${entry.path}`);
    fixturePaths.add(entry.path);

    const fixturePath = resolve(root, entry.path);
    if (!fixturePath.startsWith(`${root}/`)) fail(`fixture escaped repository root: ${entry.path}`);
    const fixtureBytes = await readFile(fixturePath);
    assertHex(entry.checked_in_fixture_sha256, 64, `${entry.path} fixture SHA-256`);
    if (sha256(fixtureBytes) !== entry.checked_in_fixture_sha256) {
      fail(`${entry.path} checked-in bytes hash mismatch`);
    }
    const fixture = JSON.parse(fixtureBytes);

    if (!Array.isArray(entry.sources) || entry.sources.length !== inventoryOrder.length) {
      fail(`${entry.path} must have one official source row per inventory`);
    }
    const sourcePointers = new Set();
    for (const source of entry.sources) {
      assertExactKeys(source, ["url", "json_pointer"], `${entry.path} source`);
      if (!source.url.startsWith("https://developers.openai.com/")) {
        fail(`${entry.path} source is not an official OpenAI developer URL`);
      }
      if (sourcePointers.has(source.json_pointer)) {
        fail(`${entry.path} repeats source pointer ${source.json_pointer}`);
      }
      sourcePointers.add(source.json_pointer);
    }

    assertExactKeys(entry.canonical_source, ["path", "format", "byte_length", "sha256"], "canonical_source");
    const sourcePathValue = entry.canonical_source.path;
    if (typeof sourcePathValue !== "string" || sourcePathValue.startsWith("/") || sourcePathValue.includes("..")) {
      fail(`${entry.path} canonical source path must be repository-relative without parent traversal`);
    }
    if (sourcePaths.has(sourcePathValue)) fail(`duplicate canonical source path ${sourcePathValue}`);
    sourcePaths.add(sourcePathValue);
    if (sourcePathValue === entry.path) fail(`${entry.path} canonical source must be a separate capture file`);
    const sourcePath = resolve(root, sourcePathValue);
    if (!sourcePath.startsWith(`${root}/`)) fail(`canonical source escaped repository root: ${sourcePathValue}`);
    const sourceBytes = await readFile(sourcePath);
    assertHex(entry.canonical_source.sha256, 64, `${entry.path} canonical source SHA-256`);
    const canonical = canonicalSource(fixture);
    if (sourceBytes.length !== entry.canonical_source.byte_length) {
      fail(`${entry.path} canonical source byte length mismatch`);
    }
    if (sha256(sourceBytes) !== entry.canonical_source.sha256) {
      fail(`${entry.path} canonical source hash mismatch`);
    }
    if (!sourceBytes.equals(canonical)) {
      fail(`${entry.path} fixture inventories differ from the independent source capture`);
    }
    if (entry.canonical_source.sha256 === entry.checked_in_fixture_sha256) {
      fail(`${entry.path} canonical-source and checked-in hashes must be independent`);
    }

    if (!Array.isArray(entry.inventories) || entry.inventories.length !== inventoryOrder.length) {
      fail(`${entry.path} inventory cardinality mismatch`);
    }
    if (JSON.stringify(entry.inventories.map((inventory) => inventory.name)) !== JSON.stringify(inventoryOrder)) {
      fail(`${entry.path} inventory order or names differ`);
    }

    for (const inventory of entry.inventories) {
      assertExactKeys(
        inventory,
        ["name", "json_pointer", "count", "sha256", "items"],
        `${entry.path} inventory`,
      );
      const expectedPointer = `/${inventory.name}`;
      if (inventory.json_pointer !== expectedPointer || !sourcePointers.has(expectedPointer)) {
        fail(`${entry.path} ${inventory.name} pointer/source mismatch`);
      }
      const actual = fixture[inventory.name];
      if (!Array.isArray(actual) || !actual.every((item) => typeof item === "string" && item.length > 0)) {
        fail(`${entry.path} ${inventory.name} is not a non-empty string inventory`);
      }
      if (actual.length !== inventory.count || inventory.items.length !== inventory.count) {
        fail(`${entry.path} ${inventory.name} count mismatch`);
      }
      if (new Set(actual).size !== actual.length || new Set(inventory.items).size !== inventory.items.length) {
        fail(`${entry.path} ${inventory.name} contains duplicates`);
      }
      if (JSON.stringify(actual) !== JSON.stringify(inventory.items)) {
        fail(`${entry.path} ${inventory.name} differs from the exact manifest inventory`);
      }
      assertHex(inventory.sha256, 64, `${entry.path} ${inventory.name} SHA-256`);
      const inventoryBytes = Buffer.from(`${actual.join("\n")}\n`, "utf8");
      if (sha256(inventoryBytes) !== inventory.sha256) {
        fail(`${entry.path} ${inventory.name} inventory hash mismatch`);
      }
      inventoryCount += 1;
      eventCount += actual.length;
    }
  }

  process.stdout.write(
    `official fixtures verified: fixtures=${manifest.fixtures.length} inventories=${inventoryCount} events=${eventCount} sdk=${manifest.sdk.package}@${manifest.sdk.version}\n`,
  );
}

await main();
