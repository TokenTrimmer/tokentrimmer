// Build an image part for later use, but do not send it to any model here.
const imagePart = {
  type: "image_url",
  image_url: { url: "data:image/jpeg;base64,/9j/4AAQSkZJRgABAQEAYABgAAD" },
};

console.log("prepared image part", imagePart.type);

export { imagePart };
