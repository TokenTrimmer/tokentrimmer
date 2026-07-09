"""D3 — client-side document distillation (the ``tt docprep`` mirror).

Read a local document, pull its text layer (PDFs, lossless — pure-Python
``pypdf``) + return a ``{"role": "user", "content": "<distilled text>"}`` chat
message. The request arrives PRE-distilled → no gateway sidecar round-trip +
routes to a text model. Mirrors ``tt docprep`` + the gateway's Document Lane
seam, reusing the same ``pypdf`` extraction so the client + the gateway agree on
what a doc distills to.

v1 scope (matches the CLI): PDF text layers only (lossless); images return an
``UnsupportedDocument`` error (OCR is out of scope for the SDK v1); remote URLs
are not fetched.

Requires the ``tokentrimmer[doc-distill]`` extra (``pip install
tokentrimmer[doc-distill]``). Without ``pypdf`` installed, ``distill_document``
/ ``user_with_document`` raise an actionable ``ImportError``.

Usage::

    from tokentrimmer import user_with_document

    messages = [user_with_document("report.pdf")]
    response = client.chat.completions.create(model="claude-haiku-4-5", messages=messages)
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Union

# The OpenAI chat-completions message shape (a plain dict — the thin-SDK
# pattern; the gateway accepts it verbatim).
ChatMessage = Dict[str, object]


@dataclass(frozen=True)
class DistilledDocument:
    """A client-side distillation result (mirrors the Rust client's shape).

    ``engine`` doubles as a status tag: ``"pypdf"`` on success.
    """

    text: str
    pages: int
    engine: str
    note: str | None


class DocumentError(Exception):
    """Base error for the document-distillation helpers."""


class UnsupportedDocument(DocumentError):
    """The media type is unsupported (non-PDF, or an unrecognized extension)."""


class EmptyExtraction(DocumentError):
    """The extraction succeeded but produced no text (a scan / an empty layer)."""


def _media_type_for(path: str | os.PathLike[str]) -> str:
    ext = Path(path).suffix.lower()
    media = {
        ".pdf": "application/pdf",
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".gif": "image/gif",
        ".webp": "image/webp",
        ".bmp": "image/bmp",
        ".tif": "image/tiff",
        ".tiff": "image/tiff",
    }.get(ext)
    if media is None:
        raise UnsupportedDocument(
            f"unrecognized file extension `{ext or '(none)'}` "
            "(expected pdf/png/jpg/gif/webp/bmp/tiff)"
        )
    return media


def distill_document(path: str | os.PathLike[str]) -> DistilledDocument:
    """Distill a local document to text (the client-side mirror of the seam).

    Reads the file + pulls its text layer (PDFs only in v1; images raise
    :class:`UnsupportedDocument`). Returns the :class:`DistilledDocument` so the
    caller can inspect the engine/note before building a message.

    Requires the ``tokentrimmer[doc-distill]`` extra (``pypdf``).
    """
    media_type = _media_type_for(path)
    if media_type != "application/pdf":
        raise UnsupportedDocument(
            f"unsupported media_type `{media_type}` "
            "(expected application/pdf; images are OCR, out of scope for the SDK v1)"
        )
    try:
        from pypdf import PdfReader
    except ImportError as err:  # pragma: no cover — the extra is documented
        raise ImportError(
            "the `tokentrimmer[doc-distill]` extra is required to distill documents "
            "(install pypdf: `pip install tokentrimmer[doc-distill]`)"
        ) from err

    with open(path, "rb") as fh:
        reader = PdfReader(fh)
        pages_text: List[str] = []
        for page in reader.pages:
            try:
                pages_text.append(page.extract_text() or "")
            except Exception as err:  # a malformed page degrades, not crashes
                pages_text.append("")
                note = f"pypdf page extract error: {err}"
                break
        else:
            note = None
    text = "\n\n".join(pages_text).strip()
    pages = len(reader.pages)
    if not text:
        raise EmptyExtraction(
            f"no text extracted (engine: pypdf, pages: {pages}, note: {note or '(none)'})"
        )
    return DistilledDocument(text=text, pages=pages, engine="pypdf", note=note)


def user_with_document(path: str | os.PathLike[str]) -> ChatMessage:
    """Build a ``user`` chat message carrying the document's distilled text.

    The request arrives pre-distilled + routes to a text model (no gateway
    sidecar round-trip). Requires the ``tokentrimmer[doc-distill]`` extra.
    """
    distilled = distill_document(path)
    return {"role": "user", "content": distilled.text}


def user_with_document_raw(path: str | os.PathLike[str]) -> ChatMessage:
    """Build a ``user`` message that attaches the document as a content part.

    For callers who want the gateway's Document Lane seam to distill it
    server-side (route opted in via ``document_lane``). No ``pypdf`` required —
    the bytes are base64-inlined in a ``file`` content part (OpenAI shape).
    """
    import base64

    media_type = _media_type_for(path)
    with open(path, "rb") as fh:
        data = base64.standard_b64encode(fh.read()).decode("ascii")
    return {
        "role": "user",
        "content": [
            {
                "type": "file",
                "file": {
                    "filename": os.path.basename(str(path)),
                    "file_data": f"data:{media_type};base64,{data}",
                },
            }
        ],
    }


# Quiet unused-import analyzers for the Union alias kept for type clarity.
_ = Union
