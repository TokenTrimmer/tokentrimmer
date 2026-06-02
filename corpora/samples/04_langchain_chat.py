"""LangChain ChatOpenAI with a bounded output."""
from langchain_openai import ChatOpenAI
from langchain_core.messages import SystemMessage, HumanMessage

llm = ChatOpenAI(model="gpt-4o-mini", max_tokens=300)

def classify(text: str) -> str:
    messages = [
        SystemMessage(content="Reply with exactly one label: spam or ham."),
        HumanMessage(content=text),
    ]
    return llm.invoke(messages).content
