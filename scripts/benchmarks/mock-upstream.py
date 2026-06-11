#!/usr/bin/env python3
"""mock-upstream.py — a tiny, zero-dependency OpenAI-compatible echo server.

PURPOSE
-------
Isolate *proxy overhead* from *provider latency*. Every gateway/proxy under
test points its upstream at THIS server instead of api.openai.com. Because the
mock returns a fixed, tiny response with (by default) zero artificial delay,
the end-to-end latency a load tester observes is dominated by the proxy's own
dispatch cost — parsing, routing, serialization, connection handling — not by a
real model generating tokens.

This is the only honest way to compare proxies head-to-head: a benchmark that
hit real providers would mostly measure OpenAI/Anthropic queue time and network
jitter, which no proxy controls and which would swamp the microseconds of
overhead we actually care about.

WIRE SURFACE (OpenAI-compatible subset)
---------------------------------------
  GET  /                       -> 200 "ok"            (liveness)
  GET  /health                 -> 200 {"status":"ok"}
  GET  /v1/models              -> 200 fixed model list
  POST /v1/chat/completions    -> 200 fixed chat completion (or SSE if stream=true)
  POST /v1/embeddings          -> 200 fixed embedding vector

Any Authorization header is accepted (the mock never validates keys — the
proxy in front of it owns auth). Any model name is echoed back.

USAGE
-----
  python3 mock-upstream.py                  # binds 127.0.0.1:8000
  MOCK_PORT=9000 python3 mock-upstream.py
  MOCK_DELAY_MS=5 python3 mock-upstream.py  # inject 5ms upstream "think" time

ENV
---
  MOCK_HOST       default 127.0.0.1   (keep loopback — this is a test fixture)
  MOCK_PORT       default 8000        (vLLM's default OpenAI port; matches
                                       TT_LOCAL_VLLM_URL=http://127.0.0.1:8000/v1)
  MOCK_DELAY_MS   default 0           (artificial per-request delay in ms; 0 =
                                       pure proxy-overhead isolation)

This server is INTENTIONALLY single-purpose and uses only the Python standard
library so it runs anywhere with python3 and needs no install step. It is a
test fixture, not production code: do not expose it to a network.
"""

import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOST = os.environ.get("MOCK_HOST", "127.0.0.1")
PORT = int(os.environ.get("MOCK_PORT", "8000"))
DELAY_MS = float(os.environ.get("MOCK_DELAY_MS", "0"))

# A fixed, minimal completion body. Token counts are made up but stable so any
# cost math downstream is deterministic. The content is short on purpose — we
# are measuring dispatch overhead, not generation throughput.
_CHAT_BODY = {
    "id": "chatcmpl-mock-0000",
    "object": "chat.completion",
    "created": 0,
    "model": "mock-model",
    "choices": [
        {
            "index": 0,
            "message": {"role": "assistant", "content": "OK"},
            "finish_reason": "stop",
        }
    ],
    "usage": {"prompt_tokens": 8, "completion_tokens": 1, "total_tokens": 9},
}

_MODELS_BODY = {
    "object": "list",
    "data": [
        {"id": "mock-model", "object": "model", "created": 0, "owned_by": "mock"}
    ],
}


def _sleep_if_configured():
    if DELAY_MS > 0:
        time.sleep(DELAY_MS / 1000.0)


class Handler(BaseHTTPRequestHandler):
    # Quiet by default — per-request logging would itself add latency and noise.
    def log_message(self, *_args):
        pass

    protocol_version = "HTTP/1.1"

    def _send_json(self, obj, status=200):
        payload = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _read_body(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length <= 0:
            return {}
        raw = self.rfile.read(length)
        try:
            return json.loads(raw)
        except (ValueError, json.JSONDecodeError):
            return {}

    def do_GET(self):
        if self.path == "/" or self.path == "":
            body = b"ok"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path == "/health":
            self._send_json({"status": "ok"})
            return
        if self.path.rstrip("/") == "/v1/models":
            self._send_json(_MODELS_BODY)
            return
        self._send_json({"error": {"message": "not found"}}, status=404)

    def do_POST(self):
        req = self._read_body()
        path = self.path.rstrip("/")

        if path == "/v1/chat/completions":
            _sleep_if_configured()
            model = req.get("model", "mock-model")
            stream = bool(req.get("stream", False))
            if stream:
                self._stream_chat(model)
            else:
                body = dict(_CHAT_BODY)
                body["model"] = model
                self._send_json(body)
            return

        if path == "/v1/embeddings":
            _sleep_if_configured()
            self._send_json(
                {
                    "object": "list",
                    "data": [
                        {"object": "embedding", "index": 0, "embedding": [0.0, 0.0, 0.0]}
                    ],
                    "model": req.get("model", "mock-embed"),
                    "usage": {"prompt_tokens": 4, "total_tokens": 4},
                }
            )
            return

        self._send_json({"error": {"message": "not found"}}, status=404)

    def _stream_chat(self, model):
        # Minimal SSE: one content delta + a stop frame + [DONE]. Chunked so we
        # don't need to precompute Content-Length.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def frame(obj):
            data = "data: " + json.dumps(obj) + "\n\n"
            chunk = data.encode("utf-8")
            self.wfile.write(b"%X\r\n" % len(chunk) + chunk + b"\r\n")

        frame(
            {
                "id": "chatcmpl-mock-0000",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [
                    {"index": 0, "delta": {"role": "assistant", "content": "OK"}, "finish_reason": None}
                ],
            }
        )
        frame(
            {
                "id": "chatcmpl-mock-0000",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            }
        )
        done = b"data: [DONE]\n\n"
        self.wfile.write(b"%X\r\n" % len(done) + done + b"\r\n")
        self.wfile.write(b"0\r\n\r\n")


def main():
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    delay_note = f"{DELAY_MS:g}ms artificial delay" if DELAY_MS > 0 else "no artificial delay"
    print(
        f"mock-upstream: OpenAI-compatible echo on http://{HOST}:{PORT}/v1 ({delay_note})",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
