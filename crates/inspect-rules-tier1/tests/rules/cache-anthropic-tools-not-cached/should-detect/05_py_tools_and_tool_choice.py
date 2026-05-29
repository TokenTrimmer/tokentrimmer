import anthropic
client = anthropic.Anthropic()
resp = client.messages.create(
    model="claude-sonnet-4-6",
    max_tokens=512,
    tools=[{"name": "db_query", "description": "Query the DB", "input_schema": {"type": "object"}}],
    tool_choice={"type": "auto"},
    messages=[{"role": "user", "content": "Query users."}],
)
