"""Installed-wheel acceptance. No source imports or network transports."""
import importlib.metadata
import json
import unittest

import httpx
from tokentrimmer import TokenTrimmer

BODY = {"model": "test-model", "messages": []}
CHUNK = {"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "test-model", "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": None}]}
SSE = ("data: " + json.dumps(CHUNK) + '\n\nevent: tokentrimmer.usage\ndata: {"cost_usd":0.01}\n\ndata: [DONE]\n\n').encode()


def fixture(options=None):
    calls = []

    def respond(request):
        calls.append(request)
        streaming = json.loads(request.content).get("stream", False)
        return httpx.Response(200, content=SSE if streaming else b'{"id":"c","object":"chat.completion","created":1,"model":"test-model","choices":[]}', headers={
            "content-type": "text/event-stream" if streaming else "application/json",
            "x-tokentrimmer-cost-usd": "0.01", "x-request-id": "req-1",
        })

    client = TokenTrimmer(api_key="tt_test_local", base_url="http://local.invalid/v1", max_retries=0, http_client=httpx.Client(transport=httpx.MockTransport(respond)), **(options or {}))
    return client, calls


class InstalledClient(unittest.TestCase):
    def test_metadata_and_headers(self):
        client, calls = fixture()
        with client:
            result = client.chat.completions.create(**BODY, tt_tag="test", tt_cost_limit=0.05, tt_cache="disabled")
            self.assertEqual(result.tt.cost_usd, 0.01)
        self.assertEqual(len(calls), 1)
        self.assertEqual(json.loads(calls[0].content), BODY)
        self.assertEqual(calls[0].headers["x-tokentrimmer-tag"], "test")
        self.assertEqual(calls[0].headers["x-tokentrimmer-cache"], "disabled")
        self.assertEqual(calls[0].headers["x-tokentrimmer-cost-limit-usd"], "0.05")

    def test_explicit_and_omitted_caps(self):
        for options, limit, expected in [
            ({}, {}, {}), ({"default_max_tokens": 32}, {}, {"max_tokens": 32}),
            ({"default_max_tokens": 32}, {"max_tokens": 8}, {"max_tokens": 8}),
            ({"default_max_tokens": 32}, {"max_completion_tokens": 16}, {"max_completion_tokens": 16}),
            ({"default_max_tokens": 32}, {"max_tokens": None}, {"max_tokens": None}),
        ]:
            with self.subTest(options=options, limit=limit):
                client, calls = fixture(options)
                with client:
                    client.chat.completions.create(**BODY, **limit)
                self.assertEqual(json.loads(calls[0].content), dict(BODY, **expected))

    def test_invalid_defaults_refused(self):
        for value in [True, False, 0, -1, 0.5, float("inf"), float("nan"), "32"]:
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "default_max_tokens"):
                TokenTrimmer(api_key="tt_test_local", default_max_tokens=value)

    def test_streaming_usage(self):
        client, calls = fixture()
        with client:
            stream = client.chat.completions.create(**BODY, stream=True)
            self.assertIsNone(stream.tt)
            chunks = list(stream)
            self.assertEqual(len(chunks), 1)
            self.assertEqual(chunks[0].choices[0].delta.content, "hi")
            self.assertEqual(stream.tt.cost_usd, 0.01)
        self.assertEqual(len(calls), 1)

    def test_inherited_raw_helpers(self):
        # These are the native OpenAI interfaces: headers/body inspection, not
        # the tt_* augmented create path. Do not promise .tt augmentation here.
        client, calls = fixture()
        with client:
            raw = client.chat.completions.with_raw_response.create(**BODY)
            self.assertEqual(raw.headers["x-tokentrimmer-cost-usd"], "0.01")
            self.assertEqual(raw.parse().id, "c")
            with client.chat.completions.with_streaming_response.create(**BODY) as response:
                self.assertEqual(response.headers["x-request-id"], "req-1")
                self.assertEqual(response.parse().id, "c")
        self.assertEqual(len(calls), 2)

    def test_stream_close_preserves_unknown_cost(self):
        closed = []

        class Feed(httpx.SyncByteStream):
            def __iter__(self):
                yield ("data: " + json.dumps(CHUNK) + "\n\n").encode()
                raise AssertionError("cancellation must not drain additional frames")

            def close(self):
                closed.append(True)

        transport = httpx.MockTransport(lambda _: httpx.Response(200, stream=Feed(), headers={"content-type": "text/event-stream"}))
        with TokenTrimmer(api_key="tt_test_local", http_client=httpx.Client(transport=transport)) as client:
            with client.chat.completions.create(**BODY, stream=True) as stream:
                self.assertEqual(next(iter(stream)).choices[0].delta.content, "hi")
            self.assertIsNone(stream.tt)
        self.assertTrue(closed)


if __name__ == "__main__":
    print("Installed wheel acceptance: openai", importlib.metadata.version("openai"), flush=True)
    unittest.main(verbosity=2)
