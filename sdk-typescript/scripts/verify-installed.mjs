// Build and install the actual npm tarball outside the repository. No publish,
// provider traffic or registry credentials are needed. Dependency install scripts
// are disabled; optional framework peers are deliberately not installed.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, cpSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const sdk = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const run = (file, args, cwd) => execFileSync(file, args, { cwd, stdio: 'inherit' });
const temp = mkdtempSync(join(tmpdir(), 'tt-sdk-installed-'));
try {
  run('npm', ['run', 'build'], sdk);
  const packed = JSON.parse(execFileSync('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', temp], { cwd: sdk, encoding: 'utf8' }));
  const archive = join(temp, packed[0].filename);
  const versions = process.argv.slice(2);
  // Floor and current compatible 6.x: each gets a separate dependency tree.
  for (const [index, version] of (versions.length ? versions : ['6.45.0', '6']).entries()) {
    const consumer = join(temp, `consumer-${index}`);
    cpSync(join(sdk, 'test-installed'), consumer, { recursive: true });
    writeFileSync(join(consumer, 'package.json'), JSON.stringify({ private: true, type: 'module' }));
    run('npm', ['install', '--ignore-scripts', '--no-audit', '--no-fund', '--package-lock=false', archive, `openai@${version}`], consumer);
    const actual = JSON.parse(readFileSync(join(consumer, 'node_modules/openai/package.json'), 'utf8')).version;
    console.log(`Installed tarball acceptance: openai ${actual}`);
    run(process.execPath, ['--test', 'tests/client.mjs'], consumer);
    run(join(sdk, 'node_modules/.bin/tsc'), ['--strict', '--noEmit', '--skipLibCheck', '--module', 'NodeNext', '--target', 'ES2022', 'tests/types.ts'], consumer);
  }
} finally {
  rmSync(temp, { recursive: true, force: true });
}
