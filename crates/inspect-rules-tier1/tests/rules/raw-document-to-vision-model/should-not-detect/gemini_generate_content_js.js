const { GoogleGenerativeAI } = require("@google/generative-ai");

const genAI = new GoogleGenerativeAI(process.env.GEMINI_API_KEY);
const model = genAI.getGenerativeModel({ model: "gemini-2.5-pro" });

async function describe() {
  const result = await model.generateContent([
    { inlineData: { mimeType: "image/png", data: "iVBORw0KGgoAAAANSUhEUgAA" } },
    "Describe the contents of this page.",
  ]);
  return result.response.text();
}

module.exports = { describe };
