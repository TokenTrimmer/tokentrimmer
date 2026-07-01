"""Shared OpenTelemetry span-recording helper for the framework integrations.

Recording the TokenTrimmer cost/savings attributes onto the current span is
identical work for every framework adapter (LangChain, LiteLLM, …), so the helper
lives here rather than being duplicated per integration.

OpenTelemetry is an **optional** extra: this module has no top-level dependency on
it — the import happens lazily inside :func:`record_on_current_span`, which is a
best-effort no-op when the ``otel`` extra is not installed. The integrations can
therefore still accumulate cost/savings totals without OpenTelemetry present.
"""

from __future__ import annotations

from typing import Any, Mapping


def record_on_current_span(attrs: Mapping[str, Any]) -> None:
    """Set ``attrs`` on the current OpenTelemetry span, if one is recording.

    When OpenTelemetry isn't installed this is a no-op, so an integration can
    record cost totals without the ``otel`` dependency. A ``NonRecordingSpan``
    (no active tracer) reports ``is_recording() == False`` and is skipped too.
    """
    try:
        from opentelemetry import trace
    except ImportError:
        return
    span = trace.get_current_span()
    if span is not None and span.is_recording():
        span.set_attributes(dict(attrs))
