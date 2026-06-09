"""Point the client at a self-hosted gateway and apply a cache override."""

import os

from tokentrimmer import TokenTrimmer

client = TokenTrimmer(
    api_key=os.environ.get("TOKENTRIMMER_API_KEY", "tt_test_local"),
    base_url=os.environ.get("TOKENTRIMMER_BASE_URL", "http://localhost:8080/v1"),
)

resp = client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Ping"}],
    max_tokens=256,
    tt_cache="bypass",  # skip the cache for this request
)
print(resp.choices[0].message.content)
print(f"cache {resp.tt.cache}")
