"""Negative: agent with max_steps configuration."""
from crewai import Agent, Crew

agent = Agent(
    role="researcher",
    goal="find information",
    backstory="expert",
    max_steps=20,
)
