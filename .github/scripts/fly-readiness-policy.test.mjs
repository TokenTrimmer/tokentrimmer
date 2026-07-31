import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), 'utf8');

test('Fly gateway manifests route traffic on readiness rather than liveness', () => {
  for (const manifest of ['fly.toml', 'fly.staging.toml']) {
    const source = read(manifest);
    const probe = source.match(/\[\[http_service\.checks\]\][\s\S]*?path\s*=\s*"([^"]+)"/);

    assert.ok(probe, `${manifest} must declare an HTTP service check`);
    assert.equal(probe[1], '/ready', `${manifest} must keep unready gateways out of rotation`);
  }
});

test('runtime keeps readiness distinct from process liveness', () => {
  const router = read('crates/core/src/server.rs');
  const health = read('crates/core/src/routes/health.rs');
  const ready = read('crates/core/src/routes/ready.rs');

  assert.match(router, /\.route\("\/health", get\(routes::health::handler\)\)/);
  assert.match(router, /\.route\("\/ready", get\(routes::ready::handler\)\)/);
  assert.match(health, /liveness-only/);
  assert.match(ready, /Postgres = HARD/);
  assert.match(ready, /StatusCode::SERVICE_UNAVAILABLE/);
});
