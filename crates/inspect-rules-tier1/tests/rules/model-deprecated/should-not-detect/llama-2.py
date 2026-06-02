from ollama import Client

client = Client(host="http://localhost:11434")
response = client.generate(
    model="llama-2:7b",
    prompt="Hello, how are you?",
    stream=False
)
