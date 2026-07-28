#!/usr/bin/env node

// Independent JavaScript verification for the generated Rust proof-contract
// fixtures. This deliberately does not import generated TypeScript or call the
// Rust canonicalizers: a mirrored canonical byte sequence catches cross-language
// drift, and re-signed forged fixtures prove structural checks precede crypto.

import {
  createHash,
  createPrivateKey,
  createPublicKey,
  sign,
  verify,
} from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const manifest = load('docs/receipt-spec/receipt-contracts.manifest.json');
const expectedContract = 'tokentrimmer.proof-contracts.v1';
const formula = 'tt.request-delta-estimate.v1';
const seed = Buffer.alloc(32, 7);
const privateKey = createPrivateKey({
  key: Buffer.concat([
    Buffer.from('302e020100300506032b657004220420', 'hex'),
    seed,
  ]),
  format: 'der',
  type: 'pkcs8',
});

assert(manifest.contract === expectedContract, 'unknown proof-contract manifest');
for (const file of manifest.files) {
  const bytes = readFileSync(join(root, file.path));
  const actual = createHash('sha256').update(bytes).digest('hex');
  assert(actual === file.sha256, `manifest checksum mismatch: ${file.path}`);
}

const fixtures = [
  ['vcr', 'docs/receipt-spec/vcr-v1.golden.json'],
  ['l2', 'docs/receipt-spec/l2-v1.golden.json'],
  ['wfr', 'docs/receipt-spec/wfr-v1.golden.json'],
  ['wfr', 'docs/receipt-spec/wfr-v2.golden.json'],
  ['wfr', 'docs/receipt-spec/wfr-v3.golden.json'],
  ['wfr', 'docs/receipt-spec/wfr-v4.golden.json'],
  ['arr', 'docs/receipt-spec/arr-v1.golden.json'],
  ['arr', 'docs/receipt-spec/arr-v2.golden.json'],
];

for (const [family, path] of fixtures) {
  const receipt = load(path);
  assert(verifyReceipt(family, receipt), `${path} must verify independently`);
  const tampered = clone(receipt);
  if (family === 'vcr') tampered.token_delta -= 1;
  if (family === 'l2') tampered.baseline_cost_usd += 0.000001;
  if (family === 'wfr' || family === 'arr') tampered.cost_micros += 1;
  assert(!verifyReceipt(family, tampered), `${path} signed-field tamper accepted`);
}

const strictWfr = load('docs/receipt-spec/wfr-v4.golden.json');
const forgedFormula = clone(strictWfr);
forgedFormula.request_delta_formula_version = 'tt.request-delta-estimate.v999';
resign('wfr', forgedFormula);
assert(!verifyReceipt('wfr', forgedFormula), 're-signed unknown formula accepted');

const forgedCoverage = clone(strictWfr);
forgedCoverage.request_delta_measured_requests -= 1;
resign('wfr', forgedCoverage);
assert(!verifyReceipt('wfr', forgedCoverage), 're-signed incomplete coverage accepted');

const forgedProjection = clone(strictWfr);
forgedProjection.saved_micros += 1;
resign('wfr', forgedProjection);
assert(!verifyReceipt('wfr', forgedProjection), 're-signed positive projection mismatch accepted');

const legacyWfr = load('docs/receipt-spec/wfr-v1.golden.json');
legacyWfr.signed_request_delta_micros = 110000;
assert(!verifyReceipt('wfr', legacyWfr), 'legacy unsigned request-delta add-on accepted');

const futureArr = load('docs/receipt-spec/arr-v2.golden.json');
futureArr.canonical_version = 'v999';
assert(!verifyReceipt('arr', futureArr), 'future ARR version accepted');

const mixedArr = load('docs/receipt-spec/arr-v1.golden.json');
mixedArr.workflow_id = '00000000-0000-0000-0000-0000000000b2';
assert(!verifyReceipt('arr', mixedArr), 'mixed ARR/WFR envelope accepted');

const bundle = load('docs/receipt-spec/savings-bundle-v1.golden.json');
assert(bundle.schema_version === 1, 'bundle fixture version drift');
assert(bundle.plan_input.seed === 42, 'bundle fixture seed drift');
assert(
  bundle.expected_result.aggregates.projected_savings_usd > 0,
  'bundle fixture no longer demonstrates positive deterministic savings',
);

console.log(`verified ${fixtures.length} signed vectors, forged cases, manifest hashes, and bundle envelope`);

function load(path) {
  return JSON.parse(readFileSync(join(root, path), 'utf8'));
}

function clone(value) {
  return JSON.parse(JSON.stringify(value));
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function verifyReceipt(family, receipt) {
  if (!validEnvelope(family, receipt)) return false;
  const payload = canonicalPayload(family, receipt);
  if (payload === null) return false;
  const signatureHex = receipt.signature_hex ?? receipt.signature;
  try {
    const key = createPublicKey({
      key: Buffer.concat([
        Buffer.from('302a300506032b6570032100', 'hex'),
        Buffer.from(receipt.verifying_key_hex, 'hex'),
      ]),
      format: 'der',
      type: 'spki',
    });
    return verify(
      null,
      Buffer.from(payload, 'utf8'),
      key,
      Buffer.from(signatureHex, 'hex'),
    );
  } catch {
    return false;
  }
}

function resign(family, receipt) {
  const payload = canonicalPayload(family, receipt);
  assert(payload !== null, `cannot canonicalize forged ${family} fixture`);
  receipt.signature_hex = sign(null, Buffer.from(payload, 'utf8'), privateKey).toString('hex');
}

function validEnvelope(family, receipt) {
  if (!receipt || typeof receipt !== 'object' || Array.isArray(receipt)) return false;
  const signatureHex = receipt.signature_hex ?? receipt.signature;
  if (!/^[0-9a-f]{64}$/i.test(receipt.verifying_key_hex ?? '')) return false;
  if (!/^[0-9a-f]{128}$/i.test(signatureHex ?? '')) return false;
  if (family === 'vcr') return receipt.schema_version === 1;
  if (family === 'l2') {
    return (
      receipt.schema_version === 1 &&
      ['confident', 'verified', 'unverifiable', 'rejected'].includes(receipt.verdict)
    );
  }
  if (!nonemptyPipeFree(receipt.status)) return false;
  if (![receipt.cost_micros, receipt.baseline_micros, receipt.saved_micros].every(Number.isSafeInteger)) {
    return false;
  }
  if (family === 'arr' && ('workflow_id' in receipt || 'quality_verdict' in receipt)) return false;
  const legacy = family === 'wfr'
    ? ['v1', 'v2'].includes(receipt.canonical_version)
    : receipt.canonical_version === 'v1';
  const strict = family === 'wfr'
    ? ['v3', 'v4'].includes(receipt.canonical_version)
    : receipt.canonical_version === 'v2';
  if (!legacy && !strict) return false;
  if (family === 'wfr') {
    const qualityVersion = ['v2', 'v4'].includes(receipt.canonical_version);
    if (qualityVersion !== nonemptyPipeFree(receipt.quality_verdict)) return false;
  }
  const deltaFields = [
    'signed_request_delta_micros',
    'request_delta_formula_version',
    'request_delta_eligible_requests',
    'request_delta_measured_requests',
  ];
  if (legacy) return deltaFields.every((name) => receipt[name] == null);
  if (receipt.request_delta_formula_version !== formula) return false;
  const eligible = receipt.request_delta_eligible_requests;
  const measured = receipt.request_delta_measured_requests;
  const signedDelta = receipt.signed_request_delta_micros;
  return (
    Number.isSafeInteger(signedDelta) &&
    Number.isSafeInteger(eligible) &&
    Number.isSafeInteger(measured) &&
    eligible > 0 &&
    measured === eligible &&
    receipt.saved_micros === Math.max(signedDelta, 0)
  );
}

function nonemptyPipeFree(value) {
  return typeof value === 'string' && value.length > 0 && !value.includes('|');
}

function canonicalPayload(family, receipt) {
  if (family === 'vcr') {
    return [
      'vcr:v1',
      receipt.schema_version,
      receipt.org_id,
      receipt.trace_id,
      receipt.route,
      receipt.model,
      receipt.token_delta,
      Math.round(receipt.savings_usd * 1_000_000),
      receipt.ts,
    ].join('|');
  }
  if (family === 'l2') {
    const similarityMicros = Math.round(
      Math.fround(Math.fround(receipt.similarity) * Math.fround(1_000_000)),
    );
    return [
      'l2:v1',
      receipt.schema_version,
      receipt.org_id,
      receipt.trace_id,
      receipt.matched_entry_id,
      similarityMicros,
      receipt.verdict,
      Math.round(receipt.served_cost_usd * 1_000_000),
      Math.round(receipt.baseline_cost_usd * 1_000_000),
      receipt.ts,
    ].join('|');
  }
  const version = receipt.canonical_version;
  if (family === 'wfr') {
    const prefix = [
      `wfr:${version}`,
      receipt.org_id,
      receipt.workflow_id,
      receipt.run_id,
    ];
    const body = runReceiptBody(receipt, ['v3', 'v4'].includes(version));
    if (body === null) return null;
    const fields = [...prefix, ...body, receipt.status];
    if (['v2', 'v4'].includes(version)) fields.push(receipt.quality_verdict);
    return fields.join('|');
  }
  if (family === 'arr') {
    const body = runReceiptBody(receipt, version === 'v2');
    if (body === null) return null;
    return [`arr:${version}`, receipt.org_id, receipt.run_id, ...body, receipt.status].join('|');
  }
  return null;
}

function runReceiptBody(receipt, strict) {
  if (!strict) {
    return [receipt.cost_micros, receipt.baseline_micros, receipt.saved_micros];
  }
  return [
    receipt.cost_micros,
    receipt.baseline_micros,
    receipt.saved_micros,
    receipt.signed_request_delta_micros,
    receipt.request_delta_formula_version,
    receipt.request_delta_eligible_requests,
    receipt.request_delta_measured_requests,
  ];
}
