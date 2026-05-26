"""Positive: Claude Sonnet for JSON schema extraction."""
import anthropic

client = anthropic.Anthropic()

def parse_invoice(raw: str) -> str:
    result = client.messages.create(
        model="claude-3-5-sonnet-20241022",
        max_tokens=512,
        system="Parse the invoice and return a structured output as JSON.",
        messages=[{"role": "user", "content": raw}],
    )
    return result.content[0].text
