"""Positive: LangChain conversation accumulating chat history."""
from langchain.chat_models import ChatOpenAI

llm = ChatOpenAI(model="gpt-4o")
conversation = []

def ask(question: str) -> str:
    conversation.append({"role": "user", "content": question})
    # ... invoke llm with full conversation ...
    response = llm.invoke(conversation)
    conversation.append({"role": "assistant", "content": response.content})
    return response.content
