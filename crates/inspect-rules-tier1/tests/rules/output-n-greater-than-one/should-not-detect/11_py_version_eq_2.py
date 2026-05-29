import openai
client = openai.OpenAI()
API_VERSION = "2"
resp = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "version is a string, no n"}],
)
