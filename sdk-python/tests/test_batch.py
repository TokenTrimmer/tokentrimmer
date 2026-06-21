"""Tests for the inherited OpenAI Batch surface routed through TokenTrimmer.

``TokenTrimmer`` subclasses ``openai.OpenAI``, so ``client.files.*`` and
``client.batches.*`` are the *native* OpenAI typed resources — we do NOT
reimplement them. Because ``base_url`` points at the TokenTrimmer Gateway and the
Gateway's ``/v1/files`` + ``/v1/batches`` endpoints are OpenAI-compatible, those
inherited calls route through TokenTrimmer transparently.

These tests PIN that behavior: they mock the Gateway's ACTUAL response shapes
(`crates/core/src/routes/batches.rs::upload_file` for the FileObject, and
`crates/core/src/batch_store.rs::BatchJob::to_openai_json` for a Batch) and
assert that (a) the inherited OpenAI surface routes to the Gateway host and (b)
the OpenAI SDK parses the response into its typed models. The gateway HTTP layer
is mocked with respx so no network is used (and the SDK's own httpx client is
exercised end to end), mirroring ``test_agent.py``.
"""

from __future__ import annotations

import json

import httpx
import respx

from tokentrimmer import TokenTrimmer

GATEWAY = "http://gw.test/v1"
FILES = f"{GATEWAY}/files"
BATCHES = f"{GATEWAY}/batches"
BATCH_ID = "batch_abc123"


def _client() -> TokenTrimmer:
    return TokenTrimmer(api_key="tt_test_x", base_url=GATEWAY)


def _file_object() -> dict:
    """The complete OpenAI FileObject the Gateway's POST /v1/files returns.

    Shape == `crates/core/src/routes/batches.rs::upload_file`: id, object,
    bytes, created_at, filename, purpose, status.
    """
    return {
        "id": "file-input123",
        "object": "file",
        "bytes": 42,
        "created_at": 1_718_000_000,
        "filename": "batch.jsonl",
        "purpose": "batch",
        "status": "processed",
    }


def _batch(status: str = "validating") -> dict:
    """A Gateway Batch body == `BatchJob::to_openai_json` for a created batch.

    The gateway emits object/id/status/created_at/request_counts always, and
    endpoint/input_file_id/completion_window for a freshly-created batch.
    """
    return {
        "id": BATCH_ID,
        "object": "batch",
        "status": status,
        "endpoint": "/v1/chat/completions",
        "input_file_id": "file-input123",
        "completion_window": "24h",
        "created_at": 1_718_000_000,
        "request_counts": {"total": 2, "completed": 0, "failed": 0},
    }


@respx.mock
def test_files_create_routes_through_gateway_and_parses_file_object():
    route = respx.post(FILES).mock(
        return_value=httpx.Response(200, json=_file_object())
    )

    f = _client().files.create(
        file=("batch.jsonl", b'{"custom_id":"req-1"}\n'),
        purpose="batch",
    )

    assert route.called
    # The inherited OpenAI surface hit the GATEWAY host (not api.openai.com).
    assert route.calls.last.request.url.host == "gw.test"
    # The complete FileObject parsed into the OpenAI typed model.
    assert f.id == "file-input123"
    assert f.object == "file"
    assert f.purpose == "batch"
    assert f.status == "processed"
    assert f.bytes == 42
    assert f.filename == "batch.jsonl"


@respx.mock
def test_batches_create_routes_through_gateway_and_parses_batch():
    route = respx.post(BATCHES).mock(
        return_value=httpx.Response(200, json=_batch("validating"))
    )

    b = _client().batches.create(
        input_file_id="file-input123",
        endpoint="/v1/chat/completions",
        completion_window="24h",
    )

    assert route.called
    assert route.calls.last.request.url.host == "gw.test"
    # The request body carries the create params.
    body = json.loads(route.calls.last.request.content)
    assert body["input_file_id"] == "file-input123"
    assert body["endpoint"] == "/v1/chat/completions"
    assert body["completion_window"] == "24h"
    # The Gateway Batch body parsed into the OpenAI typed model.
    assert b.id == BATCH_ID
    assert b.object == "batch"
    assert b.status == "validating"
    assert b.endpoint == "/v1/chat/completions"
    assert b.input_file_id == "file-input123"


@respx.mock
def test_batches_retrieve_routes_through_gateway_and_parses():
    route = respx.get(f"{BATCHES}/{BATCH_ID}").mock(
        return_value=httpx.Response(200, json=_batch("in_progress"))
    )

    b = _client().batches.retrieve(BATCH_ID)

    assert route.called
    assert route.calls.last.request.url.host == "gw.test"
    assert b.id == BATCH_ID
    assert b.status == "in_progress"
    assert b.object == "batch"


@respx.mock
def test_batches_list_routes_through_gateway_and_parses_first_item():
    # The Gateway list envelope == `list_batches`: {"object":"list","data":[...]}.
    # `has_more` is included for fidelity with the OpenAI cursor-page shape (the
    # OpenAI SDK treats it as optional).
    route = respx.get(BATCHES).mock(
        return_value=httpx.Response(
            200,
            json={
                "object": "list",
                "data": [_batch("completed")],
                "has_more": False,
            },
        )
    )

    page = _client().batches.list()

    assert route.called
    assert route.calls.last.request.url.host == "gw.test"
    # `.list()` returns a paginated page; the first item parses into a Batch.
    items = list(page)
    assert items[0].id == BATCH_ID
    assert items[0].status == "completed"
    # `.data` accessor agrees with iteration.
    assert page.data[0].id == BATCH_ID


@respx.mock
def test_batches_cancel_routes_through_gateway_and_parses():
    route = respx.post(f"{BATCHES}/{BATCH_ID}/cancel").mock(
        return_value=httpx.Response(200, json=_batch("cancelling"))
    )

    b = _client().batches.cancel(BATCH_ID)

    assert route.called
    assert route.calls.last.request.url.host == "gw.test"
    assert b.id == BATCH_ID
    assert b.status == "cancelling"
