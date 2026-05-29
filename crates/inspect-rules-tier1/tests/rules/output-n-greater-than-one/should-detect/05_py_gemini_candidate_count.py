import google.generativeai as genai
model = genai.GenerativeModel("gemini-3.1-pro")
resp = model.generate_content(
    "Brainstorm",
    generation_config={"candidate_count": 4},
)
