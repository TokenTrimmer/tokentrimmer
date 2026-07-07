# An image_url part is built and inspected, but never passed to an LLM call.
part = {
    "type": "image_url",
    "image_url": {"url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA"},
}

print(part["type"])
print(len(part["image_url"]["url"]))
