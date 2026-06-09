"""Non-streaming chat that prints the TokenTrimmer cost attribution."""

import os

from tokentrimmer import TokenTrimmer

client = TokenTrimmer(api_key=os.environ["TOKENTRIMMER_API_KEY"])

resp = client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Say hello in five words."}],
    max_tokens=256,
    tt_tag="example=cost-attribution",
    tt_cost_limit=0.05,
)

print(resp.choices[0].message.content)
print(f"cost      ${resp.tt.cost_usd}")
print(f"baseline  ${resp.tt.baseline_cost_usd}")
print(f"saved     ${resp.tt.saved_usd}")
print(f"cache     {resp.tt.cache}")
