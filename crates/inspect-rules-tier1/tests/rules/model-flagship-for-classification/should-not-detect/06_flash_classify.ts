// Negative: Gemini Flash (small model) for classification.
import { GoogleGenerativeAI } from "@google/generative-ai";

const genAI = new GoogleGenerativeAI(process.env.GOOGLE_API_KEY!);
const model = genAI.getGenerativeModel({ model: "gemini-2.0-flash" });

async function classify(text: string) {
  const result = await model.generateContent(`Classify sentiment: ${text}`);
  return result.response.text();
}
