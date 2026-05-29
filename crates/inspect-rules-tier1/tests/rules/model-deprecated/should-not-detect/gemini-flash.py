import google.generativeai as genai

client = genai.GenerativeModel("gemini-2.0-flash")
response = client.generate_content("Hello")
