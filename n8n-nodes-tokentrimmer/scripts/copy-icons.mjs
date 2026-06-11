// Copies nodes/**/*.svg into dist/nodes/** preserving relative paths.
// Cross-platform (plain node:fs) — no gulp or shell globbing.
import { copyFileSync, mkdirSync, readdirSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const pkgRoot = join(dirname(fileURLToPath(import.meta.url)), '..');
const srcRoot = join(pkgRoot, 'nodes');
const distRoot = join(pkgRoot, 'dist', 'nodes');

let copied = 0;
for (const entry of readdirSync(srcRoot, { recursive: true, withFileTypes: true })) {
	if (!entry.isFile() || !entry.name.endsWith('.svg')) continue;
	const src = join(entry.parentPath, entry.name);
	const dest = join(distRoot, relative(srcRoot, src));
	mkdirSync(dirname(dest), { recursive: true });
	copyFileSync(src, dest);
	copied += 1;
}
console.log(`copy-icons: copied ${copied} svg file(s) to dist/nodes/`);
