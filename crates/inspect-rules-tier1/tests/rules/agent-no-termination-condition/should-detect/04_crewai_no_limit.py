"""Positive: crewai agent with while True loop and no iteration cap."""
from crewai import Agent, Task, Crew

agent = Agent(role="researcher", goal="research the topic", backstory="expert")

while True:
    task = Task(description="research AI", agent=agent)
    crew = Crew(agents=[agent], tasks=[task])
    result = crew.kickoff()
    if result.is_final:
        break
