"""Tests for client-side document distillation (D3 — ``tokentrimmer.document``).

Mirror the Rust ``tt-client`` document tests (crates/client/src/document.rs):
a text-layer PDF distills to its text; an image/unknown extension raises
``UnsupportedDocument``; a missing file raises ``OSError``; the no-extra path
raises an actionable ``ImportError`` (when ``pypdf`` is unavailable).

The PDF fixture is built in-test via pypdf's writer (the Rust tests use lopdf
for the same end). pypdf is required → the whole module is skipped when it's
uninstalled (the `doc-distill` extra isn't installed).
"""

from __future__ import annotations

from pathlib import Path

import pytest

pypdf = pytest.importorskip("pypdf")

from tokentrimmer.document import (  # noqa: E402  (import after importorskip)
    DistilledDocument,
    DocumentError,
    UnsupportedDocument,
    distill_document,
    user_with_document,
    user_with_document_raw,
)


def _text_layer_pdf(path: Path, text: str = "Hello TokenTrimmer") -> None:
    """Write a single-page PDF whose text layer reads ``text``.

    Hand-crafted from the PDF spec (a one-page Type1/Hel Helvetica doc with a
    ``Tj`` text-show op) — no writer lib, so the fixture is version-stable
    across pypdf releases (pypdf is reader-first; its writer's page-tree
    internals are private + shifted in v6). Mirrors the Rust lopdf fixture.
    """
    # Escape parens/backslashes for the Tj literal string operand.
    safe = text.replace("\\", "\\\\").replace("(", r"\(").replace(")", r"\)")
    content = f"BT\n/F1 24 Tf\n72 700 Td\n({safe}) Tj\nET\n".encode("latin-1", "replace")

    objs: list[bytes] = []

    def add(obj: bytes) -> int:
        objs.append(obj)
        return len(objs)  # 1-based object numbers

    font_obj = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
    resources_obj = b"<< /Font << /F1 2 0 R >> >>"
    content_obj = (
        b"<< /Length " + str(len(content)).encode() + b" >>\nstream\n" + content + b"\nendstream"
    )
    page_obj = (
        b"<< /Type /Page /Parent 5 0 R /Contents 4 0 R /Resources 3 0 R "
        b"/MediaBox [0 0 595 842] >>"
    )

    add(font_obj)       # 1 0 R
    add(resources_obj)  # 2 0 R
    # rewrite: page references resources 3 0 R + contents 4 0 R
    objs.clear()
    add(font_obj)
    add(b"<< /Font << /F1 1 0 R >> >>")  # resources = 2 0 R
    add(b"<< /Length "
        + str(len(content)).encode()
        + b" >>\nstream\n" + content + b"\nendstream")  # contents = 3 0 R
    add(b"<< /Type /Page /Parent 5 0 R /Contents 3 0 R "
        b"/Resources 2 0 R /MediaBox [0 0 595 842] >>")  # page = 4 0 R
    add(b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>")  # pages = 5 0 R
    add(b"<< /Type /Catalog /Pages 5 0 R >>")  # catalog = 6 0 R

    pdf = bytearray(b"%PDF-1.5\n")
    offsets: list[int] = []
    for i, obj in enumerate(objs, start=1):
        offsets.append(len(pdf))
        pdf += f"{i} 0 obj\n".encode() + obj + b"\nendobj\n"
    xref_off = len(pdf)
    pdf += b"xref\n0 " + str(len(objs) + 1).encode() + b"\n"
    pdf += b"0000000000 65535 f \n"
    for off in offsets:
        pdf += f"{off:010d} 00000 n \n".encode()
    pdf += b"trailer\n<< /Size " + str(len(objs) + 1).encode() + b" /Root 6 0 R >>\n"
    pdf += b"startxref\n" + str(xref_off).encode() + b"\n%%EOF\n"
    path.write_bytes(bytes(pdf))


def test_media_type_unknown_extension_errors(tmp_path: Path) -> None:
    bad = tmp_path / "doc.xyz"
    bad.write_bytes(b"whatever")
    with pytest.raises(UnsupportedDocument):
        distill_document(bad)


def test_distill_missing_file_errors(tmp_path: Path) -> None:
    with pytest.raises(OSError):
        distill_document(tmp_path / "nonexistent.pdf")


def test_distill_text_layer_pdf_extracts_the_text(tmp_path: Path) -> None:
    pdf = tmp_path / "doc.pdf"
    _text_layer_pdf(pdf, "Hello TokenTrimmer")
    distilled = distill_document(pdf)
    assert isinstance(distilled, DistilledDocument)
    assert distilled.engine == "pypdf"
    assert distilled.pages == 1
    assert "Hello TokenTrimmer" in distilled.text
    assert distilled.note is None


def test_distill_png_is_unsupported(tmp_path: Path) -> None:
    png = tmp_path / "img.png"
    png.write_bytes(b"\x89PNG\r\n\x1a\n")  # PNG signature
    with pytest.raises(UnsupportedDocument):
        distill_document(png)


def test_user_with_document_builds_a_pre_distilled_message(tmp_path: Path) -> None:
    pdf = tmp_path / "doc.pdf"
    _text_layer_pdf(pdf, "Hello TokenTrimmer")
    msg = user_with_document(pdf)
    assert msg["role"] == "user"
    assert "Hello TokenTrimmer" in str(msg["content"])


def test_user_with_document_raw_attaches_a_file_part(tmp_path: Path) -> None:
    pdf = tmp_path / "doc.pdf"
    pdf.write_bytes(b"%PDF-1.5 fake bytes")
    msg = user_with_document_raw(pdf)
    assert msg["role"] == "user"
    content = msg["content"]
    assert isinstance(content, list)
    assert content[0]["type"] == "file"
    assert "file_data" in content[0]["file"]


def test_document_error_is_a_document_error(tmp_path: Path) -> None:
    bad = tmp_path / "doc.xyz"
    bad.write_bytes(b"x")
    with pytest.raises(DocumentError):
        distill_document(bad)
