"""Negative: Gemini generate_content with max_output_tokens in config."""
import google.generativeai as genai

model = genai.GenerativeModel("gemini-2.0-pro")
response = model.generate_content(
    "What is AI?",
    generation_config=genai.types.GenerationConfig(max_output_tokens=1024),
)
