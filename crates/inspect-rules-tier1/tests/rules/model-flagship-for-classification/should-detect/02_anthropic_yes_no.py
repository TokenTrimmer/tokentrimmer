"""Positive: Claude Opus used for yes or no classification."""
import anthropic

client = anthropic.Anthropic()

def is_spam(message: str) -> bool:
    result = client.messages.create(
        model="claude-3-opus-20240229",
        max_tokens=5,
        system="Answer yes or no only.",
        messages=[{"role": "user", "content": f"Is this spam? {message}"}],
    )
    return "yes" in result.content[0].text.lower()
