from anthropic import Anthropic
client = Anthropic()
resp = client.messages.create(
    model="claude-opus-4-7",
    max_tokens=2048,
    tools=[
        {"name": "search", "description": "Search the web", "input_schema": {"type": "object"}},
        {"name": "calc", "description": "Do math", "input_schema": {"type": "object"}},
    ],
    messages=[{"role": "user", "content": "Find and compute."}],
)
