import google.generativeai as genai
model = genai.GenerativeModel("gemini-3.1-pro")
resp = model.generate_content("List APIs", tools=[{"function_declarations": []}])
