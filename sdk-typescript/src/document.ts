/**
 * D3 — client-side document distillation (the `tt docprep` mirror).
 *
 * Read a local document, pull its text layer (PDFs, lossless — pure-JS
 * `pdf-parse`) + return a `{ role: 'user', content: '<distilled text>' }` chat
 * message. The request arrives PRE-distilled → no gateway sidecar round-trip +
 * routes to a text model. Mirrors `tt docprep` + the gateway's Document Lane
 * seam, reusing the same extraction so the client + the gateway agree on what a
 * doc distills to.
 *
 * v1 scope (matches the CLI): PDF text layers only (lossless); images throw an
 * `UnsupportedDocumentError` (OCR is out of scope for the SDK v1); remote URLs
 * are not fetched.
 *
 * Requires `unpdf` as a peer: `npm install unpdf`. Without it installed,
 * `distillDocument` throws an actionable `Error`. `userWithDocumentRaw` needs no
 * extra (it base64-inlines the bytes for the gateway seam).
 *
 * @example
 *
 * ```ts
 * import { userWithDocument } from '@tokentrimmer/client';
 *
 * const messages = [await userWithDocument('report.pdf')];
 * const response = await client.chat.completions.create({
 *   model: 'claude-haiku-4-5',
 *   messages,
 * });
 * ```
 *
 * @module
 */

import { readFile } from 'node:fs/promises';
import { extname, basename } from 'node:path';

/** A client-side distillation result (mirrors the Rust + Python shapes). */
export interface DistilledDocument {
  /** The extracted text (all pages joined). */
  text: string;
  /** Number of pages the extractor saw. */
  pages: number;
  /** The engine / status tag (`'pdf-parse'` on success). */
  engine: string;
  /** A human-readable note (why a result is empty). `null` on success. */
  note: string | null;
}

/** Base error for the document-distillation helpers. */
export class DocumentError extends Error {}
/** The media type is unsupported (non-PDF, or an unrecognized extension). */
export class UnsupportedDocumentError extends DocumentError {}
/** The extraction succeeded but produced no text (a scan / an empty layer). */
export class EmptyExtractionError extends DocumentError {}

const IMAGE_EXTS = new Map<string, string>([
  ['.png', 'image/png'],
  ['.jpg', 'image/jpeg'],
  ['.jpeg', 'image/jpeg'],
  ['.gif', 'image/gif'],
  ['.webp', 'image/webp'],
  ['.bmp', 'image/bmp'],
  ['.tif', 'image/tiff'],
  ['.tiff', 'image/tiff'],
]);

function mediaTypeFor(path: string): string {
  const ext = extname(path).toLowerCase();
  if (ext === '.pdf') return 'application/pdf';
  const image = IMAGE_EXTS.get(ext);
  if (image) return image;
  throw new UnsupportedDocumentError(
    `unrecognized file extension \`${ext || '(none)'}\` (expected pdf/png/jpg/gif/webp/bmp/tiff)`,
  );
}

/**
 * Distill a local document to text (the client-side mirror of the seam). Pulls
 * the text layer (PDFs only in v1; images throw {@link UnsupportedDocumentError}).
 * Returns the {@link DistilledDocument} so the caller can inspect the engine/note
 * before building a message.
 *
 * Requires the `pdf-parse` package.
 */
export async function distillDocument(path: string): Promise<DistilledDocument> {
  const mediaType = mediaTypeFor(path);
  if (mediaType !== 'application/pdf') {
    throw new UnsupportedDocumentError(
      `unsupported media_type \`${mediaType}\` (expected application/pdf; images are OCR, out of scope for the SDK v1)`,
    );
  }
  let extract: (
    data: Uint8Array,
  ) => Promise<{ text: string; pages: number }>;
  try {
    // `unpdf` is a maintained, ESM-first PDF text extractor (built on the
    // edge-compatible pdfjs). The default export is the parse fn: it takes the
    // raw bytes and returns `{ text, totalPages }`.
    const mod = await import('unpdf');
    const parse = mod.extractText;
    extract = async (data) => {
      const pdf = await mod.getDocumentProxy(data);
      const r = await parse(pdf, { mergePages: true });
      return { text: r.text, pages: r.totalPages };
    };
  } catch {
    throw new Error(
      'the `unpdf` package is required to distill documents (install it: `npm install unpdf`)',
    );
  }
  const bytes = await readFile(path);
  let result: { text: string; pages: number };
  try {
    result = await extract(new Uint8Array(bytes));
  } catch (err) {
    throw new DocumentError(`pdf parse error: ${(err as Error).message}`);
  }
  const text = (result.text ?? '').trim();
  if (!text) {
    throw new EmptyExtractionError(
      `no text extracted (engine: unpdf, pages: ${result.pages})`,
    );
  }
  return {
    text,
    pages: result.pages,
    engine: 'unpdf',
    note: null,
  };
}

/**
 * Build a `user` chat message carrying the document's distilled text. The
 * request arrives pre-distilled + routes to a text model (no gateway sidecar
 * round-trip). Requires the `pdf-parse` package.
 */
export async function userWithDocument(path: string): Promise<{
  role: 'user';
  content: string;
}> {
  const distilled = await distillDocument(path);
  return { role: 'user', content: distilled.text };
}

/**
 * Build a `user` message that attaches the document as a `file` content part
 * (OpenAI shape) — for callers who want the gateway's Document Lane seam to
 * distill it server-side. No `pdf-parse` required.
 */
export async function userWithDocumentRaw(path: string): Promise<{
  role: 'user';
  content: Array<{ type: 'file'; file: { filename: string; file_data: string } }>;
}> {
  const mediaType = mediaTypeFor(path);
  const bytes = await readFile(path);
  const data = bytes.toString('base64');
  return {
    role: 'user',
    content: [
      {
        type: 'file',
        file: {
          filename: basename(path),
          file_data: `data:${mediaType};base64,${data}`,
        },
      },
    ],
  };
}
