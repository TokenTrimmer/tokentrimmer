"""Negative: while True but has budget check — termination condition."""
import openai

client = openai.OpenAI()

def run_agent(task: str, budget: float = 1.0):
    messages = [{"role": "user", "content": task}]
    spent = 0.0
    while True:
        if spent > budget:
            break
        response = client.chat.completions.create(
            model="gpt-4o", messages=messages, tools=TOOLS
        )
        spent += estimate_cost(response)
        if response.choices[0].finish_reason == "stop":
            break
