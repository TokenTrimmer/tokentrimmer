// Date.now() in the system message content.
const messages = [
  { role: "system", content: `You are an assistant. Request id ${Date.now()} applies.` },
];
module.exports = { messages };
