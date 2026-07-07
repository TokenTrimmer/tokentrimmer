"""Helper that assembles a vision message but leaves the API call to the caller."""


def build_image_message(data_url: str) -> dict:
    return {
        "role": "user",
        "content": [
            {"type": "text", "text": "Analyze this."},
            {"type": "image_url", "image_url": {"url": data_url}},
        ],
    }


MESSAGE = build_image_message("data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA")
print(MESSAGE["role"])
