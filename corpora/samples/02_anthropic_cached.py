"""Anthropic call with prompt caching on the system block and tools."""
import anthropic

client = anthropic.Anthropic()

SYSTEM = "You are an expert assistant. " + ("Follow these standing rules. " * 400)

def ask(question: str):
    return client.messages.create(
        model="claude-sonnet-4-6",
        max_tokens=1024,
        system=[{"type": "text", "text": SYSTEM, "cache_control": {"type": "ephemeral"}}],
        tools=[{"name": "search", "description": "Search", "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"}}],
        messages=[{"role": "user", "content": question}],
    )
