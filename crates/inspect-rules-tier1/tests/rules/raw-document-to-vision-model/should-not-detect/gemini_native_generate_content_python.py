import google.generativeai as genai

# Native Google SDK: the model id lives on the client object, not the call.
# Gemini prices page-images flat, so the projector books no saving -> suppressed.
model = genai.GenerativeModel("gemini-2.5-flash")

resp = model.generate_content(
    [
        {"inline_data": {"mime_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAA"}},
        "Describe this receipt in detail.",
    ]
)
print(resp.text)
