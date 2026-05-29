import openai

openai.api_key = "sk-test"
response = openai.Completion.create(
    model="text-davinci-002",
    prompt="Complete this"
)
