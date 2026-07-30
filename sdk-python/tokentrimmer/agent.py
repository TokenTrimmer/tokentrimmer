"""Driver for the server-side agent loop (``POST /v1/agent/runs``).

Unlike a single chat completion — where the caller drives every
model->tool->model round-trip from the client — the Gateway's agent-run
endpoint owns the loop server-side (mid-loop down-routing, judge-gated
summarize, substep cache). The client's job is therefore narrower: kick off a
run, and whenever the run pauses on a CLIENT (non-gateway) tool, execute it via
the caller's ``executor`` and resume by POSTing the tool outputs, until the run
reaches a terminal answer.

This mirrors the Rust ``tt-client`` driver (crates/client/src/agent.rs).

Cost differs from chat: the agent endpoint returns a single JSON :class:`Run`
whose ``usage`` aggregates served cost across every server-side turn — there are
NO per-turn ``x-tokentrimmer-*`` headers — so the aggregate cost is read from the
response body (``run.usage.cost_usd``), not the headers.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional
from urllib.parse import quote

import httpx

# A caller-supplied client-tool executor: ``(name, arguments) -> output``.
# ``arguments`` is the raw JSON-string the model produced; ``output`` is the raw
# string fed back to the model as the tool result. Raising is allowed — the
# driver catches it and feeds the error back as the tool output (never aborts).
Executor = Callable[[str, str], str]

# Resume (``tool_outputs``) round-trip cap, matching the Rust driver default. A
# guard against a model that loops forever on client tools — independent of the
# server-side ``max_turns``.
DEFAULT_MAX_RESUME_ROUNDS = 8


@dataclass(frozen=True)
class RunUsage:
    """Accumulated token usage + served cost across every turn of a run.

    ``cost_usd`` is the SUM of each turn's served cost; it is the agent-loop
    analogue of a chat response's ``.tt.cost_usd``.
    """

    prompt_tokens: int = 0
    completion_tokens: int = 0
    cost_usd: float = 0.0

    @classmethod
    def _from_payload(cls, data: Optional[Dict[str, Any]]) -> "RunUsage":
        data = data or {}
        return cls(
            prompt_tokens=int(data.get("prompt_tokens", 0)),
            completion_tokens=int(data.get("completion_tokens", 0)),
            cost_usd=float(data.get("cost_usd", 0.0)),
        )


@dataclass(frozen=True)
class Run:
    """A run record returned by ``POST /v1/agent/runs`` and the resume endpoint.

    The full ``messages`` transcript is included so the caller sees the whole
    model/tool exchange. ``status`` is one of ``completed`` / ``incomplete`` /
    ``failed`` / ``requires_action``; only ``requires_action`` is resumable.
    """

    id: str
    status: str
    messages: List[Dict[str, Any]]
    turns: int
    usage: RunUsage
    note: Optional[str] = None
    summarizer_tax_usd: Optional[float] = None

    @classmethod
    def _from_payload(cls, data: Dict[str, Any]) -> "Run":
        return cls(
            id=str(data.get("id", "")),
            status=str(data.get("status", "")),
            messages=list(data.get("messages", [])),
            turns=int(data.get("turns", 0)),
            usage=RunUsage._from_payload(data.get("usage")),
            note=data.get("note"),
            summarizer_tax_usd=data.get("summarizer_tax_usd"),
        )

    @property
    def is_terminal(self) -> bool:
        """True once the run can no longer be resumed (anything but ``requires_action``)."""
        return self.status != "requires_action"

    @property
    def text(self) -> Optional[str]:
        """The final assistant text (last assistant message's string content), if any."""
        for msg in reversed(self.messages):
            if msg.get("role") == "assistant":
                content = msg.get("content")
                if isinstance(content, str):
                    return content
        return None

    def pending_tool_calls(self) -> List[Dict[str, Any]]:
        """The CLIENT tool calls the run is paused on.

        These are assistant ``tool_calls`` in the transcript that have NO
        answering ``tool`` message. The Gateway answers its own (read-only)
        gateway tool calls inline before pausing, so the unanswered remainder
        are exactly the client tools the caller must run. Empty unless
        ``status == "requires_action"``.
        """
        answered = {
            msg.get("tool_call_id")
            for msg in self.messages
            if msg.get("role") == "tool"
        }
        pending: List[Dict[str, Any]] = []
        for msg in self.messages:
            if msg.get("role") != "assistant":
                continue
            for call in msg.get("tool_calls") or []:
                if call.get("id") not in answered:
                    pending.append(call)
        return pending


@dataclass(frozen=True)
class AgentOutcome:
    """The result of driving an agent run to a terminal state (or the resume cap)."""

    run: Run
    #: Number of ``POST .../tool_outputs`` resume round-trips the driver made.
    resume_rounds: int

    @property
    def text(self) -> Optional[str]:
        """The run's final answer text, if any. See :attr:`Run.text`."""
        return self.run.text

    @property
    def usage(self) -> RunUsage:
        """Aggregate token usage + served cost across the whole run."""
        return self.run.usage

    @property
    def messages(self) -> List[Dict[str, Any]]:
        """The full final transcript of the run."""
        return self.run.messages


def _executor_error_output(exc: BaseException) -> str:
    """Render an executor exception as the JSON tool output fed back to the model.

    Mirrors the Rust driver's ``{"error": "..."}`` payload so the model can react
    to a failed tool rather than the whole run aborting.
    """
    return json.dumps({"error": str(exc)})


class Agent:
    """Server-side agent-loop driver, exposed as ``client.agent``.

    Reuses the SDK's existing base URL, API key, and underlying httpx client (no
    new HTTP dependency, and the same transport the chat path uses — so it is
    mockable with the same test harness).
    """

    def __init__(self, client: Any) -> None:
        # The owning ``TokenTrimmer`` (an ``openai.OpenAI`` subclass). We read
        # ``base_url`` / ``api_key`` and the underlying sync httpx client off it.
        self._client = client

    @property
    def _base(self) -> str:
        # ``client.base_url`` is an httpx.URL ending in a slash (e.g.
        # ``http://host/v1/``); strip it so we can join clean path segments.
        return str(self._client.base_url).rstrip("/")

    @property
    def _http(self) -> httpx.Client:
        # The OpenAI SDK's configured sync httpx client (respx-mockable).
        return self._client._client  # type: ignore[no-any-return]

    def run(
        self,
        *,
        model: str,
        messages: List[Dict[str, Any]],
        executor: Executor,
        tools: Optional[List[Dict[str, Any]]] = None,
        max_turns: Optional[int] = None,
        max_resume_rounds: int = DEFAULT_MAX_RESUME_ROUNDS,
        tt_tag: Optional[str] = None,
    ) -> AgentOutcome:
        """Drive the agent run to completion.

        Create the run, then while it is paused on client tools, execute them via
        ``executor`` and resume — until a terminal status or the resume cap.

        :param model: the model id to route the run with (required).
        :param messages: the OpenAI-shaped conversation to seed the run.
        :param executor: ``(name, arguments) -> output``; raising is fed back as
            the tool's output (the run does not abort). See :data:`Executor`.
        :param tools: function tools advertised to the model (OpenAI shape). A
            run only pauses on tools the Gateway does NOT execute itself.
        :param max_turns: server-side per-run turn cap (the Gateway clamps to
            ``[1, 32]``); ``None`` uses the Gateway default.
        :param max_resume_rounds: client-side cap on resume round-trips
            (default 8) before the driver returns a still-paused run.
        :param tt_tag: ``X-TokenTrimmer-Tag`` cost-attribution tag, forwarded on
            the create AND every resume request.
        :raises ValueError: if ``model`` is empty.
        :raises httpx.HTTPStatusError: on a gateway/transport failure of the
            create or a resume call (a gateway failure aborts the run).
        """
        if not model or not model.strip():
            raise ValueError("model must be a non-empty string")

        run = self._create(model, messages, tools, max_turns, tt_tag)
        resume_rounds = 0

        while run.status == "requires_action" and resume_rounds < max_resume_rounds:
            pending = run.pending_tool_calls()
            # A ``requires_action`` run always has >=1 pending client tool; an
            # empty list would mean a server contract break. Stop rather than
            # POST an empty resume the Gateway would reject.
            if not pending:
                break
            outputs = []
            for call in pending:
                fn = call.get("function") or {}
                name = str(fn.get("name", ""))
                arguments = str(fn.get("arguments", ""))
                try:
                    output = executor(name, arguments)
                except Exception as exc:  # noqa: BLE001 - feed error back, never abort
                    output = _executor_error_output(exc)
                outputs.append({"tool_call_id": call.get("id"), "output": output})
            run = self._resume(run.id, outputs, tt_tag)
            resume_rounds += 1

        return AgentOutcome(run=run, resume_rounds=resume_rounds)

    def export_transcript(self, run_id: str) -> Run:
        """Export a still-retained paused/resumed transcript.

        The Gateway returns 404 after the one-hour Redis record expires or is
        deleted. Durable run metadata remains available through run history.
        """
        url = self._transcript_url(run_id)
        resp = self._http.get(url, headers=self._headers())
        resp.raise_for_status()
        return Run._from_payload(resp.json())

    def delete_transcript(self, run_id: str) -> None:
        """Idempotently erase retained transcript/resume state only."""
        url = self._transcript_url(run_id)
        resp = self._http.delete(url, headers=self._headers())
        resp.raise_for_status()

    def _create(
        self,
        model: str,
        messages: List[Dict[str, Any]],
        tools: Optional[List[Dict[str, Any]]],
        max_turns: Optional[int],
        tt_tag: Optional[str],
    ) -> Run:
        body: Dict[str, Any] = {"model": model, "messages": messages, "stream": False}
        if tools:
            body["tools"] = tools
        if max_turns is not None:
            body["max_turns"] = max_turns
        return self._send(f"{self._base}/agent/runs", body, tt_tag)

    def _resume(
        self, run_id: str, outputs: List[Dict[str, Any]], tt_tag: Optional[str]
    ) -> Run:
        body = {"tool_outputs": outputs}
        url = f"{self._base}/agent/runs/{run_id}/tool_outputs"
        return self._send(url, body, tt_tag)

    def _send(self, url: str, body: Dict[str, Any], tt_tag: Optional[str]) -> Run:
        headers = self._headers()
        if tt_tag is not None:
            headers["X-TokenTrimmer-Tag"] = str(tt_tag)
        resp = self._http.post(url, json=body, headers=headers)
        resp.raise_for_status()
        return Run._from_payload(resp.json())

    def _headers(self) -> Dict[str, str]:
        return {
            "Authorization": f"Bearer {self._client.api_key}",
            "Content-Type": "application/json",
        }

    def _transcript_url(self, run_id: str) -> str:
        if not isinstance(run_id, str) or not run_id.strip():
            raise ValueError("run_id must be a non-empty string")
        return f"{self._base}/agent/runs/{quote(run_id, safe='')}/transcript"
