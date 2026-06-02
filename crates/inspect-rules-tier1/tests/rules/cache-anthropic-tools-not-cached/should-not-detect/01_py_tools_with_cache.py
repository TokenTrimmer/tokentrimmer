import anthropic
client = anthropic.Anthropic()
resp = client.messages.create(
    model="claude-sonnet-4-6",
    max_tokens=1024,
    tools=[{"name": "get_weather", "description": "Get weather", "input_schema": {"type": "object"},
            "cache_control": {"type": "ephemeral"}}],
    messages=[{"role": "user", "content": "Weather?"}],
)
