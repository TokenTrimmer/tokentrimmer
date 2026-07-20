#!/usr/bin/env node

// Dependency-free cross-language smoke for generated route/workflow/capability
// artifacts. Rust owns generation and semantic validation; this independently
// checks manifest hashes plus the TypeScript/schema/vector seams consumers use.

import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const manifest = load('docs/contracts/product-contracts.manifest.json');
assert(manifest.contract === 'tokentrimmer.product-contracts.v1', 'unknown product manifest');

const families = new Map(manifest.contracts.map((contract) => [contract.family, contract]));
for (const family of ['route', 'workflow_definition', 'workflow_write', 'gateway_capabilities']) {
  assert(families.has(family), `missing product contract family: ${family}`);
}

for (const file of manifest.files) {
  assert(
    typeof file.path === 'string' &&
      !file.path.startsWith('/') &&
      !file.path.includes('\\') &&
      !file.path.split('/').includes('..'),
    'unsafe product-contract manifest path',
  );
  assert(/^[a-f0-9]{64}$/.test(file.sha256), `invalid SHA-256 for ${file.path}`);
  const actual = createHash('sha256').update(readFileSync(join(root, file.path))).digest('hex');
  assert(actual === file.sha256, `manifest checksum mismatch: ${file.path}`);
}

const route = load(families.get('route').schema);
assert(route.$id === 'urn:tokentrimmer:route:write-schema:v1', 'route schema id drift');
assert(route.additionalProperties === false, 'route write root must reject unknown fields');
assert(
  JSON.stringify(route.properties.schema_version.enum) === JSON.stringify([1, null]),
  'route schema version boundary drift',
);
assert(route.$defs.RouteAction.additionalProperties === false, 'route action must reject unknown fields');
assert('content_compress' in route.$defs.RouteAction.properties, 'live route action field missing');

const workflow = load(families.get('workflow_definition').schema);
const workflowWrite = load(families.get('workflow_write').schema);
assert(workflow.$id === 'urn:tokentrimmer:workflow:definition-schema:v1', 'workflow schema id drift');
assert(workflowWrite.$id === 'urn:tokentrimmer:workflow:write-schema:v1', 'workflow write id drift');
assert(workflow.required.includes('id') && workflow.required.includes('version'), 'stored identity missing');
assert(!workflowWrite.required.includes('id') && !workflowWrite.required.includes('version'), 'write identity is not optional');
assert(workflow.$defs.Node.properties.id.type === 'string', 'workflow node id missing');
assert(workflow.$defs.Node.oneOf.length === 10, 'workflow node family count drift');

const vector = load(families.get('workflow_definition').vectors[0]);
assert(vector.version === 1, 'workflow vector version drift');
assert(
  JSON.stringify(vector.nodes.map((node) => node.type)) ===
    JSON.stringify(['trigger', 'model', 'output']),
  'workflow vector node sequence drift',
);
assert(vector.nodes[1].max_output_tokens === 256, 'workflow output-token cap missing');
assert(vector.triggers[0].type === 'schedule', 'workflow trigger missing');

const capabilities = load(families.get('gateway_capabilities').schema);
assert(capabilities.properties.schema_version.const === 1, 'capability version drift');
assert(capabilities.properties.scope.const === 'gateway_runtime', 'capability scope drift');
assert(
  capabilities.$defs.NumericLimit.properties.value.minimum === 1,
  'capability positive member limit missing',
);

const typescript = readText(manifest.typescript);
for (const typeName of [
  'RouteWriteRequest',
  'WorkflowDefinition',
  'WorkflowWriteRequest',
  'GatewayCapabilitiesDocument',
]) {
  assert(typescript.includes(`export type ${typeName} =`), `missing generated ${typeName}`);
}
assert(typescript.includes('export type Node = {\n  id: string;\n} & ('), 'flattened node id lost');
assert(!/\bany\b/.test(typescript), 'generated product TypeScript contains any');

console.log(`verified ${manifest.files.length} generated product files across ${families.size} families`);

function load(path) {
  return JSON.parse(readText(path));
}

function readText(path) {
  return readFileSync(join(root, path), 'utf8');
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}
