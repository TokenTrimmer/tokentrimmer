import openai

system = """Translate English to French.

Example:
Input: Hello
Output: Bonjour

Example:
Input: Good morning
Output: Bon matin

Example:
Input: Thank you
Output: Merci

Example:
Input: How are you?
Output: Comment allez-vous?

Example:
Input: I am fine
Output: Je vais bien

Example:
Input: Nice to meet you
Output: Enchante de vous rencontrer

Example:
Input: What is your name?
Output: Quel est votre nom?

Example:
Input: I don't understand
Output: Je ne comprends pas"""

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4",
    system=system,
    messages=[{"role": "user", "content": "Hello"}]
)
