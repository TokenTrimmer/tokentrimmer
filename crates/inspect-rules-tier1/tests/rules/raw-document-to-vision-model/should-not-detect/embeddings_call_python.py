import openai

client = openai.OpenAI()

# An embeddings create call over plain text: no image part, not a vision call.
resp = client.embeddings.create(
    model="text-embedding-3-small",
    input="Quarterly earnings summary for the finance team.",
)
vector = resp.data[0].embedding
print(len(vector))
