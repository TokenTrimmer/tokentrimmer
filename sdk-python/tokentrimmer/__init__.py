"""TokenTrimmer Python SDK.

A thin wrapper around the official OpenAI SDK that routes requests through
the TokenTrimmer Gateway. The Gateway accepts the OpenAI Chat Completions
schema verbatim, so the SDK is structurally identical to ``openai.OpenAI``
with three additions:

1. ``base_url`` defaults to ``https://api.tokentrimmer.com/v1``.
2. ``tt_tag`` keyword on ``chat.completions.create`` sets the
   ``X-TokenTrimmer-Tag`` header for cost attribution.
3. ``.tt`` metadata accessor on responses surfaces the
   ``X-TokenTrimmer-*`` headers (cost, baseline_cost, saved, cache, provider,
   model_used, trace_id) without parsing them manually.

Usage::

    from tokentrimmer import TokenTrimmer

    client = TokenTrimmer(api_key="tt_live_...")

    response = client.chat.completions.create(
        model="claude-haiku-4-5",
        messages=[{"role": "user", "content": "Hello"}],
        max_tokens=1024,
        tt_tag="feature=chat-support",
    )

    print(response.choices[0].message.content)
    print(f"Cost: ${response.tt.cost_usd:.4f}  Saved: ${response.tt.saved_usd:.4f}")
    print(f"Cache: {response.tt.cache}  Trace: {response.tt.trace_id}")
"""

from tokentrimmer.client import TokenTrimmer, TokenTrimmerMeta

__all__ = ["TokenTrimmer", "TokenTrimmerMeta"]
__version__ = "0.1.0"
