import { describe, expect, it } from 'vitest';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  distillDocument,
  userWithDocument,
  userWithDocumentRaw,
  UnsupportedDocumentError,
} from '../src/document.js';

// `unpdf` is an optional peer — skip the parse tests when it's absent (the real
// round-trip is exercised in CI where it's installed).
const hasUnpdf = await import('unpdf')
  .then(() => true)
  .catch(() => false);
const itIfPdf = hasUnpdf ? it : it.skip;

/**
 * Write a single-page PDF whose text layer reads `text` — hand-crafted from the
 * PDF spec (a one-page Helvetica doc with a `Tj` text-show op). Version-stable
 * across pdf-parse releases (no writer lib), mirroring the Rust lopdf fixture +
 * the Python hand-crafted fixture.
 */
function textLayerPdf(text: string): Buffer {
  const safe = text.replace(/\\/g, '\\\\').replace(/\(/g, '\\(').replace(/\)/g, '\\)');
  const content = `BT\n/F1 24 Tf\n72 700 Td\n(${safe}) Tj\nET\n`;
  // Objects (1=font, 2=resources, 3=contents, 4=page, 5=pages, 6=catalog).
  const objs = [
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
    '<< /Font << /F1 1 0 R >> >>',
    `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
    '<< /Type /Page /Parent 5 0 R /Contents 3 0 R /Resources 2 0 R /MediaBox [0 0 595 842] >>',
    '<< /Type /Pages /Kids [4 0 R] /Count 1 >>',
    '<< /Type /Catalog /Pages 5 0 R >>',
  ];
  // Build with string concatenation so the xref byte offsets are exact (Buffer
  // concatenation length-tracking is error-prone). Verified round-trips through
  // pdf-parse + pypdf (mirrors the Python hand-crafted fixture).
  let pdf = '%PDF-1.5\n';
  const offsets: number[] = [];
  objs.forEach((obj, i) => {
    offsets.push(Buffer.byteLength(pdf));
    pdf += `${i + 1} 0 obj\n${obj}\nendobj\n`;
  });
  const xrefOff = Buffer.byteLength(pdf);
  pdf += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n`;
  offsets.forEach((off) => {
    pdf += `${String(off).padStart(10, '0')} 00000 n \n`;
  });
  pdf += `trailer\n<< /Size ${objs.length + 1} /Root 6 0 R >>\nstartxref\n${xrefOff}\n%%EOF\n`;
  return Buffer.from(pdf, 'latin1');
}

async function tmpDir(): Promise<string> {
  return mkdtemp(join(tmpdir(), 'tt-client-d3-'));
}

describe('document distillation (D3)', () => {
  it('rejects an unknown extension', async () => {
    const dir = await tmpDir();
    const path = join(dir, 'doc.xyz');
    await writeFile(path, 'whatever');
    await expect(distillDocument(path)).rejects.toBeInstanceOf(UnsupportedDocumentError);
    await rm(dir, { recursive: true, force: true });
  });

  it('rejects a missing file', async () => {
    await expect(distillDocument(join(tmpdir(), 'nonexistent-doc-d3.pdf'))).rejects.toThrow();
  });

  itIfPdf('distills a text-layer PDF to its text', async () => {
    const dir = await tmpDir();
    const path = join(dir, 'doc.pdf');
    await writeFile(path, textLayerPdf('Hello TokenTrimmer'));
    const distilled = await distillDocument(path);
    expect(distilled.engine).toBe('unpdf');
    expect(distilled.pages).toBeGreaterThanOrEqual(1);
    expect(distilled.text).toContain('Hello TokenTrimmer');
    expect(distilled.note).toBeNull();
    await rm(dir, { recursive: true, force: true });
  });

  itIfPdf('rejects a PNG as unsupported', async () => {
    const dir = await tmpDir();
    const path = join(dir, 'img.png');
    await writeFile(path, Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
    await expect(distillDocument(path)).rejects.toBeInstanceOf(UnsupportedDocumentError);
    await rm(dir, { recursive: true, force: true });
  });

  it('builds a pre-distilled user message', async () => {
    // userWithDocument delegates to distillDocument (skipped w/o pdf-parse), so
    // test the raw helper here which needs no extra.
    const dir = await tmpDir();
    const path = join(dir, 'doc.pdf');
    await writeFile(path, Buffer.from('%PDF-1.5 fake bytes'));
    const msg = await userWithDocumentRaw(path);
    expect(msg.role).toBe('user');
    expect(Array.isArray(msg.content)).toBe(true);
    expect(msg.content[0]!.type).toBe('file');
    expect(msg.content[0]!.file.file_data).toContain('data:application/pdf;base64,');
    await rm(dir, { recursive: true, force: true });
  });

  itIfPdf('userWithDocument returns a user message carrying the distilled text', async () => {
    const dir = await tmpDir();
    const path = join(dir, 'doc.pdf');
    await writeFile(path, textLayerPdf('Hello TokenTrimmer'));
    const msg = await userWithDocument(path);
    expect(msg.role).toBe('user');
    expect(typeof msg.content).toBe('string');
    expect(msg.content).toContain('Hello TokenTrimmer');
    await rm(dir, { recursive: true, force: true });
  });
});
