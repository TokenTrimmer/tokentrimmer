import anthropic

system = """You are a sentiment classifier.

Example: Input: "I love this product" Output: positive
Example: Input: "This is great" Output: positive  
Example: Input: "Amazing quality" Output: positive
Example: Input: "I hate this" Output: negative
Example: Input: "Terrible experience" Output: negative
Example: Input: "Very bad" Output: negative
Example: Input: "It's okay" Output: neutral
Example: Input: "Not bad" Output: neutral
Example: Input: "Could be better" Output: neutral
Example: Input: "The best ever" Output: positive
Example: Input: "Worst purchase" Output: negative
Example: Input: "So-so product" Output: neutral"""

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-haiku-20240307",
    system=system,
    messages=[{"role": "user", "content": "Is this good?"}]
)
