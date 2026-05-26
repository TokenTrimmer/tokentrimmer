// Positive: GPT-4o for true or false determination.
import OpenAI from "openai";

const openai = new OpenAI();

async function isToxic(text: string): Promise<boolean> {
  const r = await openai.chat.completions.create({
    model: "gpt-4o",
    messages: [
      { role: "system", content: "Respond true or false only." },
      { role: "user", content: `Is this text toxic? ${text}` },
    ],
  });
  return r.choices[0].message.content?.toLowerCase() === "true";
}
