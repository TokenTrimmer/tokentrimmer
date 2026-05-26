"""Positive: Gemini call without max_output_tokens."""
import google.generativeai as genai

model = genai.GenerativeModel("gemini-2.0-pro")
response = model.generate_content("Explain general relativity in detail.")
